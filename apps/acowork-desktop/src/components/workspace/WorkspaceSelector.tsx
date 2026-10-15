import { useState, useEffect, useLayoutEffect, useRef, useCallback } from "react";
import { createPortal } from "react-dom";
import { useAgentStore } from "../../stores/agentStore";
import { useSettingsStore } from "../../stores/settingsStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useChatStore } from "../../stores/chatStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { WorkspaceDir } from "../../stores/workspaceStore";
import { useToast } from "../common/ToastProvider";
import { FolderOpen, FolderPlus, Trash2, Shield, ShieldOff, Home, Check } from "lucide-react";
import * as dialog from "@tauri-apps/plugin-dialog";
import { cn } from "../../lib/utils";
import { computeDropdownPlacement } from "../../lib/dropdownPosition";
import { menuItemHover, menuItemSelected } from "../../lib/ui-styles";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { Tooltip } from "../common/Tooltip";
import { RemoteFolderPicker } from "./RemoteFolderPicker";

function workspaceDisplayName(dir: WorkspaceDir) {
  const name = (dir.alias?.trim() || dir.path).split(/[\\/]+/).filter(Boolean).pop() || dir.path;
  return name;
}

export function WorkspaceSelector({ dropDirection = "up", textHidden, readOnly }: { dropDirection?: "up" | "down"; textHidden?: boolean; readOnly?: boolean }) {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  // Node id of the machine hosting the selected agent instance — when set,
  // the RemoteFolderPicker browses that node's filesystem via the
  // Gateway's `/api/fs/browse?target=...` reverse proxy (ADR-055 L7-1).
  // Undefined for locally-spawned agents on the Gateway machine, in which
  // case the picker falls back to browsing the Gateway filesystem (same
  // machine, equivalent result).
  const browseTargetNodeId = useAgentStore((s) =>
    selectedAgentId ? s.agents[selectedAgentId]?.meta?.node_id : undefined,
  );
  // Note: we intentionally do NOT gate `fetchWorkspaces` on
  // `s.agents[selectedAgentId]?.meta?.lifecycle` here (ADR-085;
  // formerly `meta.ready`). Bug B v3 fix — the lifecycle push arrives
  // asynchronously to Runtime HTTP readiness. Gating on it caused the
  // workspace dropdown to flash "Loading…" forever when the user opened
  // the workspace panel during the first second after agent start.
  // Instead, the store's `fetchWorkspaces` now owns the 503 retry loop
  // (see `lib/httpRetry.ts`) so transient 503s during Runtime port
  // registration recover transparently.
  const { gatewayUrl, gatewayMode } = useSettingsStore();
  const { addToast } = useToast();
  const { workspaces, sessionWorkspaceMap, loading, deniedReason, fetchWorkspaces, setSessionWorkspace } =
    useWorkspaceStore();
  const activeSessionId = useChatStore((s) => {
    if (!selectedAgentId) return null;
    return s.getActiveSessionId(selectedAgentId);
  });
  const [open, setOpen] = useState(false);
  const [searchQuery, _setSearchQuery] = useState("");
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const [showRemotePicker, setShowRemotePicker] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  const menuRef = useRef<HTMLDivElement>(null);
  const [menuPos, setMenuPos] = useState<{ top: number; left: number } | null>(null);

  // Current workspace ID for the active session (defaults to agent home)
  const currentWsId = activeSessionId
    ? (sessionWorkspaceMap[activeSessionId] ?? "__agent_home__")
    : "__agent_home__";

  // Fetch workspaces whenever the selected agent changes. The store
  // owns the 503 retry loop — see `lib/httpRetry.ts` and the comment
  // at the top of this component — so we do not need to gate on
  // agentReady (the MQTT-retained ready flag is asynchronous to
  // Runtime HTTP readiness and gating on it caused the workspace
  // dropdown to hang at "Loading…" forever).
  useEffect(() => {
    if (!selectedAgentId) {
      useWorkspaceStore.getState().reset();
      return;
    }
    void fetchWorkspaces(selectedAgentId);
  }, [selectedAgentId, fetchWorkspaces]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      const target = e.target as Node;
      // The menu is portalled to document.body, so it is NOT a descendant
      // of the trigger wrapper — testing only `ref` would close the menu on
      // the very first mousedown inside it.
      if (ref.current?.contains(target) || menuRef.current?.contains(target)) return;
      setOpen(false);
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // ── Dropdown placement ────────────────────────────────────────────
  // The menu used to be `absolute left-0` inside the trigger's `.relative`
  // wrapper. That is fine while the anchor sits in a wide column, but the
  // workspace panel is user-resizable and can be dragged narrower than the
  // menu's fixed `w-80` — the menu was then cropped, because the whole
  // right panel sits inside `CAPSULE_PANE_CN` (`overflow-hidden`), so the
  // overflow was clipped by the pane, not merely hidden.
  //
  // Fix mirrors ProfileTab / AgentSetupTab: portal the menu to
  // `document.body` with `position: fixed`, then place it by measurement.
  // Anchor on the trigger's RIGHT edge and pull left when the menu would
  // cross it — i.e. the menu's right edge stays flush with the trigger's
  // right edge and it grows leftward, out of the narrow panel.
  //
  // ponytail: recomputed on resize + capture-phase scroll only, so the
  // ceiling is "a panel drag moves the trigger but the user must reopen
  // the menu". Upgrade path: observe `ref.current` with a
  // ResizeObserver if live-tracking during a drag is ever needed.
  const computeMenuPos = useCallback(() => {
    const anchorEl = ref.current;
    const menuEl = menuRef.current;
    if (!anchorEl || !menuEl) return;
    const r = anchorEl.getBoundingClientRect();
    // offsetWidth/Height, not getBoundingClientRect on the menu: the
    // placement is inline `left/top`, so there is no transform to cancel.
    const placement = computeDropdownPlacement({
      anchor: { left: r.left, right: r.right, top: r.top, bottom: r.bottom },
      menuWidth: menuEl.offsetWidth,
      menuHeight: menuEl.offsetHeight,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
      direction: dropDirection,
    });
    setMenuPos({ top: placement.top, left: placement.left });
  }, [dropDirection]);

  // Measure synchronously before paint: the menu must never be visible at
  // its un-measured (0,0) position, otherwise it flashes in the corner.
  useLayoutEffect(() => {
    if (open) computeMenuPos();
    else setMenuPos(null);
  }, [open, computeMenuPos]);

  // Re-measure when the menu's own size changes. The list fills in
  // asynchronously ("Loading..." -> rows), and with `dropDirection="up"`
  // (the chat toolbar) the height feeds straight into `top` — without this
  // the menu would sit at the height it had while still loading.
  useLayoutEffect(() => {
    const menuEl = menuRef.current;
    if (!open || !menuEl || typeof ResizeObserver === "undefined") return;
    const ro = new ResizeObserver(() => computeMenuPos());
    ro.observe(menuEl);
    return () => ro.disconnect();
  }, [open, computeMenuPos]);

  useEffect(() => {
    if (!open) return;
    computeMenuPos();
    window.addEventListener("resize", computeMenuPos);
    // Capture-phase: catches the workspace panel's own scroll containers,
    // not just the window, so the menu stays glued to the trigger.
    window.addEventListener("scroll", computeMenuPos, true);
    return () => {
      window.removeEventListener("resize", computeMenuPos);
      window.removeEventListener("scroll", computeMenuPos, true);
    };
  }, [open, computeMenuPos]);

  const handleSelect = async (dir: WorkspaceDir) => {
    if (!selectedAgentId || !activeSessionId) return;
    await setSessionWorkspace(selectedAgentId, activeSessionId, dir.id);
    setOpen(false);
  };

  const handleBrowse = async () => {
    // Relay mode: server-side fs browse is blocked by the Gateway's
    // remote ACL (design doc 24 §7.2 — `/api/fs/browse` answers 404 to
    // relay-originated requests), and the local dialog is meaningless
    // for a remote workspace host. The browse button is disabled; the
    // user picks workspaces from the list or via agent-side paths.
    if (gatewayMode === "relay") {
      return;
    }
    // Remote (LAN) mode: use RemoteFolderPicker to browse server filesystem
    if (gatewayMode === "remote") {
      setShowRemotePicker(true);
      return;
    }
    // Local mode: use Tauri native directory dialog
    try {
      const selected = await dialog.open({ directory: true });
      if (selected && selectedAgentId) {
        const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({
            path: selected,
            alias: selected.split(/[\/]/).filter(Boolean).pop() || undefined,
            access: "read-only",
          }),
        });
        if (response.ok) {
          addToast({ type: "success", message: "Workspace added" });
          void fetchWorkspaces(selectedAgentId);
        }
      }
    } catch {
      // User cancelled
    }
  };

  const handleDelete = useCallback(async (id: string, name: string) => {
    if (!selectedAgentId || deletingId) return;
    setDeletingId(id);
    try {
      // If the deleted workspace is the current session's workspace, fallback to agent home
      if (currentWsId === id && activeSessionId) {
        await setSessionWorkspace(selectedAgentId, activeSessionId, "__agent_home__");
      }
      const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces/${id}`, {
        method: "DELETE",
      });
      if (response.ok) {
        addToast({ type: "success", message: `Removed ${name}` });
        void fetchWorkspaces(selectedAgentId);
      } else {
        addToast({ type: "error", message: "Failed to remove workspace" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to remove workspace: ${String(error)}` });
    } finally {
      setDeletingId(null);
    }
    setConfirmDelete(null);
  }, [selectedAgentId, gatewayUrl, addToast, fetchWorkspaces, deletingId, currentWsId, activeSessionId, setSessionWorkspace]);

  const handleToggleAccess = useCallback(async (dir: WorkspaceDir) => {
    if (!selectedAgentId) return;
    const newAccess = dir.access === "read-only" ? "read-write" : "read-only";
    try {
      const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces/${dir.id}`, {
        method: "PUT",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ access: newAccess }),
      });
      if (response.ok) {
        addToast({ type: "success", message: `Access changed to ${newAccess === "read-write" ? "RW" : "RO"}` });
        void fetchWorkspaces(selectedAgentId);
      } else {
        addToast({ type: "error", message: "Failed to update access" });
      }
    } catch (error) {
      addToast({ type: "error", message: `Failed to update access: ${String(error)}` });
    }
  }, [selectedAgentId, gatewayUrl, addToast, fetchWorkspaces]);

  const filteredWorkspaces = workspaces.filter((w) =>
    !searchQuery.trim() ||
    w.path.toLowerCase().includes(searchQuery.toLowerCase()) ||
    (w.alias && w.alias.toLowerCase().includes(searchQuery.toLowerCase())),
  );

  const currentWsName = currentWsId === "__agent_home__"
    ? "Agent Home"
    : currentWsId
      ? (() => {
        const w = workspaces.find((ws) => ws.id === currentWsId);
        if (!w) return t("workspace.defaultWorkspaceName");
        const name = workspaceDisplayName(w);
        return name.length > 24 ? name.slice(0, 24) + "..." : name;
      })()
      : t("workspace.defaultWorkspaceName");

  return (
    <>
      <ToolbarDropdownTrigger
        icon={<FolderOpen size={14} />}
        label={currentWsName}
        collapseClass="tb-ws-text"
        tipClass="tb-ws-tip"
        tooltip={readOnly ? t("chatPanel.readOnlySession") : t("workspace.selectWorkspace")}
        open={open}
        onToggle={() => !readOnly && setOpen(!open)}
        wrapperRef={ref}
        textHidden={textHidden}
        btnId="ws"
        disabled={readOnly}
      >
        {/* Dropdown menu — portalled to document.body.
            The right panel sits inside `CAPSULE_PANE_CN` (`overflow-hidden`),
            so the old `absolute left-0 w-80` menu was *clipped by the pane*
            whenever the user dragged the panel narrower than 320px — not
            merely hidden. `position: fixed` + measured coords (menuPos,
            see computeMenuPos) lifts it above every clip; right-anchoring
            lets it grow leftward, out of the panel, instead of being cut.
            The portal target is document.body, so keeping this JSX inside
            the trigger's children has no effect on where it paints. */}
        {open && createPortal(
          <div
            ref={menuRef}
            className={cn(
              "w-80 max-w-[calc(100vw-2rem)] rounded-md border border-border-outer bg-modal-surface shadow-lg",
              // Stay hidden until the layout effect has measured us,
              // otherwise the first frame paints at (0,0).
              menuPos ? "visible" : "invisible",
            )}
            style={{
              position: "fixed",
              top: menuPos?.top ?? 0,
              left: menuPos?.left ?? 0,
              zIndex: 100,
            }}
          >
            {/* Menu title */}
            <div className="px-3 pt-2.5 pb-1">
              <h2 className="text-sm font-normal text-text-secondary ">
                {t("workspace.title")}
              </h2>
            </div>
            <div className="border-t border-border-divider" />

            {/* Workspace list */}
            <div className="max-h-[420px] overflow-y-auto py-1">
              {/* Agent Home — always first, cannot be deleted */}
              <button
                onClick={async () => {
                  if (!selectedAgentId || !activeSessionId) return;
                  await setSessionWorkspace(selectedAgentId, activeSessionId, "__agent_home__");
                  setOpen(false);
                }}
                className={cn(
                  "flex w-full items-center gap-2 px-3 py-1.5 text-text transition-colors",
                  currentWsId === "__agent_home__" ? menuItemSelected : menuItemHover,
                )}
              >
                <Home className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                <div className="min-w-0 flex-1 text-left">
                  <div className={cn("truncate text-xs", currentWsId === "__agent_home__" && "font-semibold")}>
                    Agent Home
                  </div>
                  <div className="truncate text-10 text-text-tertiary ">
                    {t("workspace.defaultWorkingDirectory")}
                  </div>
                </div>
                {currentWsId === "__agent_home__" && (
                  <Check size={12} className="shrink-0 text-[var(--color-accent)]" />
                )}
              </button>

              {/* Divider */}
              <div className="mx-3 my-1 border-t border-border-divider" />

              {loading ? (
                <div className="py-4 text-center text-xs text-text-tertiary">{t("workspace.loading")}</div>
              ) : deniedReason === "manage" ? (
                // ADR-087: the empty list here is a 403, not "this agent
                // has no workspaces". Without this the panel rendered
                // "未配置工作区" — actively wrong, it told the caller to
                // add one, which they are not allowed to do.
                <div className="px-4 py-4 text-center">
                  <div className="text-xs text-text-secondary">{t("workspace.deniedTitle")}</div>
                  <div className="mt-1 text-10 leading-relaxed text-text-tertiary">
                    {t("workspace.deniedHint")}
                  </div>
                </div>
              ) : filteredWorkspaces.length === 0 ? (
                <div className="py-4 text-center text-xs text-text-tertiary">
                  {searchQuery ? t("workspace.noMatchingWorkspaces") : t("workspace.noWorkspacesConfigured")}
                </div>
              ) : (
                <div className="space-y-0.5">
                  {filteredWorkspaces.map((dir) => {
                    const isCurrent = dir.id === currentWsId;
                    const isDeleting = confirmDelete === dir.id;
                    const displayName = workspaceDisplayName(dir);

                    return (
                      <div
                        key={dir.id}
                        className={cn(
                          "group flex items-center gap-2 px-3 py-1.5 transition-colors",
                          isCurrent ? menuItemSelected : menuItemHover,
                        )}
                      >
                        {/* Select workspace button */}
                        <button
                          onClick={() => handleSelect(dir)}
                          className="flex min-w-0 flex-1 items-center gap-2 text-left"
                        >
                          <FolderOpen className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                          <div className="min-w-0 flex-1">
                            <div className="truncate text-xs font-medium text-text-secondary ">
                              {displayName}
                            </div>
                            <Tooltip content={dir.path} variant="plain" position="bottom">
                              <div className="truncate text-10 text-text-tertiary ">
                                {dir.path}
                              </div>
                            </Tooltip>
                          </div>
                          {/* Sibling of the stacked name/path column, NOT
                              inside it: this column is a block box, so an
                              inline svg here lands on its own line under the
                              path. As a flex item of the button it inherits
                              items-center and sits beside the text. */}
                          {isCurrent && <Check size={12} className="shrink-0 text-[var(--color-accent)]" />}
                        </button>

                        {/* Action buttons: delete (reversible) + access toggle */}
                        <div className="flex shrink-0 items-center gap-1">
                          {/* Delete button */}
                          {isDeleting ? (
                            <div className="flex items-center gap-1">
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  void handleDelete(dir.id, displayName);
                                }}
                                disabled={deletingId !== null}
                className="rounded px-2 py-0.5 text-xs text-white disabled:opacity-50 disabled:cursor-not-allowed"
                style={{ backgroundColor: "var(--color-accent-strong)" }}
                              >
                                {t("workspace.delete")}
                              </button>
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setConfirmDelete(null);
                                }}
                                className="rounded bg-zinc-200 px-2 py-0.5 text-xs text-text-secondary hover:bg-zinc-300 dark:bg-zinc-600  dark:hover:bg-zinc-500"
                              >
                                {t("workspace.cancel")}
                              </button>
                            </div>
                          ) : (
                            <Tooltip content={t("workspace.removeWorkspace")} variant="plain">
                              <button
                                onClick={(e) => {
                                  e.stopPropagation();
                                  setConfirmDelete(dir.id);
                                }}
                                disabled={deletingId !== null}
                                className="rounded p-1 text-text-tertiary opacity-0 transition-all group-hover:opacity-100 hover:bg-zinc-100 dark:hover:bg-zinc-700 disabled:opacity-50 disabled:cursor-not-allowed"
                                style={{}}
                              >
                                <Trash2 className="h-3 w-3" />
                              </button>
                            </Tooltip>
                          )}

                          {/* Access toggle button */}
                          <Tooltip content={dir.access === "read-write" ? t("workspace.changeToReadOnly") : t("workspace.changeToReadWrite")} variant="plain">
                          <button
                              onClick={(e) => {
                                e.stopPropagation();
                                void handleToggleAccess(dir);
                              }}
                              className="flex items-center gap-1 rounded px-1.5 py-0.5 text-11 transition-colors hover:bg-zinc-200 dark:hover:bg-zinc-600"
                            >
                              {dir.access === "read-write" ? (
                                <>
                                  <ShieldOff className="h-3 w-3 text-orange-600 dark:text-orange-400" />
                                  <span className="font-medium text-orange-700 dark:text-orange-400">RW</span>
                                </>
                              ) : (
                                <>
                                  <Shield className="h-3 w-3 text-text-tertiary" />
                                  <span className="font-medium text-text-secondary ">RO</span>
                                </>
                              )}
                            </button>
                          </Tooltip>
                        </div>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>

            {/* Divider */}
            <div className="border-t border-border-divider" />

            {/* Add workspace button */}
            <button
              onClick={handleBrowse}
              // ADR-087: mutation is manage-tier; a caller who was just
              // denied the list has no business offering the add path
              // either. One gate, both buttons.
              disabled={gatewayMode === "relay" || deniedReason === "manage"}
              className="mx-3 mt-2 mb-2.5 flex w-[calc(100%-1.5rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-200 hover:text-zinc-900 disabled:opacity-40 disabled:hover:bg-zinc-100 dark:bg-white/10  dark:hover:bg-white/15 dark:hover:text-zinc-100"
            >
              <FolderPlus className="h-3.5 w-3.5" />
              {t("workspace.addWorkspace")}
            </button>
          </div>,
          document.body,
        )}
      </ToolbarDropdownTrigger>

      {/* Remote folder picker (only shown in remote mode) */}
      {showRemotePicker && selectedAgentId && (
        <RemoteFolderPicker
          target={browseTargetNodeId}
          onSelect={async (path: string) => {
            setShowRemotePicker(false);
            try {
              const response = await fetch(`${gatewayUrl}/api/agents/${selectedAgentId}/workspaces`, {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({
                  path,
                  alias: path.split("/").filter(Boolean).pop() || undefined,
                  access: "read-only",
                }),
              });
              if (response.ok) {
                addToast({ type: "success", message: "Workspace added" });
                void fetchWorkspaces(selectedAgentId);
              } else {
                const err = await response.json().catch(() => null);
                addToast({ type: "error", message: err?.error || "Failed to add workspace" });
              }
            } catch (error) {
              addToast({ type: "error", message: `Failed to add workspace: ${String(error)}` });
            }
          }}
          onCancel={() => setShowRemotePicker(false)}
        />
      )}
    </>
  );
}
