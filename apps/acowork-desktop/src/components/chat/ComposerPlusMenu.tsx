import { useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState } from "react";
import { createPortal } from "react-dom";
import {
  Sparkles,
  Plug,
  Wrench,
  Paperclip,
  ChevronLeft,
  ChevronRight,
  RotateCcw,
  Loader2,
} from "lucide-react";
import { cn } from "../../lib/utils";
import { computeDropdownPlacement, DROPDOWN_EDGE_GAP } from "../../lib/dropdownPosition";
import { menuItemHover, toolbarIconButton } from "../../lib/ui-styles";
import { Tooltip } from "../common/Tooltip";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { Switch } from "../common/Switch";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { selectPolicyEntry, useToolPolicyStore } from "../../stores/toolPolicyStore";
import {
  deriveBuiltinState,
  deriveMcpServerState,
  deriveMcpToolState,
  type AgentToolInventory,
  type SessionToolPolicy,
} from "../../lib/toolInventory";
import { useTranslation } from "../../i18n/useTranslation";
import { SkillMenuList } from "../skills/SkillMenuList";

/**
 * The composer's `+` menu — one trigger, four things the input can carry.
 *
 * # Why a drill-down column instead of a horizontal flyout
 *
 * The composer is a narrow, bottom-anchored strip inside a pane that can be
 * dragged very small. A nested flyout (level 2 opening to the right of level 1,
 * level 3 to the right of level 2) needs three panel widths of horizontal room
 * and silently clips the third one in exactly the window size users drag
 * themselves into. So each level replaces the previous one *in the same
 * fixed-size box*: predictable geometry, one scroll container, and the back
 * chevron makes the depth obvious.
 *
 * # Level 1 → 2 → 3
 *
 *   Skills      → the skill list (pick = the next message carries `skill_name`)
 *   MCP Servers → server switches, chevron → that server's tool switches
 *   Builtin     → tool switches
 *   Attachments → not a panel: closes the menu and opens the OS file picker
 *
 * # What the switches mean
 *
 * These are the **session** layer, not the agent layer: each switch can only
 * take away what `agent-mcp.json` / `agent-tools.json` already grants, never add
 * to it (`lib/toolInventory.ts`, module doc). A tool the agent itself disables
 * therefore shows an off, disabled switch — the honest presentation of "you
 * cannot widen this here" — and the group header offers `跟随 Agent 默认` to drop
 * the session's narrowing entirely.
 *
 * Every write goes through `useToolPolicyStore`, which persists only the
 * changed group and leaves a never-touched session at `null` (inheriting).
 */
type PlusView =
  | { kind: "root" }
  | { kind: "skills" }
  | { kind: "builtin" }
  | { kind: "mcp" }
  | { kind: "mcpTools"; server: string };

/**
 * The `+` trigger's mark.
 *
 * Drawn here instead of taken from lucide for one measurable reason: lucide
 * gives every glyph a 24-unit box but fills it unevenly, and `Plus` is a light
 * case — its paths run 5→19 on each axis. Rendered at the 16 this cluster's
 * icon-only buttons use (`Send`, `Paperclip`, `Eye`, the usage ring), that is
 * only ~10.7px of ink at a 1.33px stroke, which is the "the + is barely
 * visible" tell.
 *
 * The mark is therefore specified in *ink* rather than in box size, and it
 * lands between the ring and the rest of the row:
 *
 *     usage ring            16.0px span, 2.00px stroke  (the loud end)
 *     this cross            12.5px span, 1.50px stroke
 *     lucide Plus at 16     10.7px span, 1.33px stroke  (the quiet end)
 *
 * Matching the ring exactly was tried first and reads as abrupt: the cross then
 * carries more ink and a heavier stroke than every other glyph in the strip, so
 * it stops being one control among peers and becomes the loudest thing on the
 * row. ~78% of the ring's span at 75% of its weight is still clearly the
 * largest mark in the left cluster without shouting over the ring.
 *
 * Geometry: a 16 box (the cluster's size, so the button and the fold engine's
 * measurement are untouched), 1.5 stroke, round caps on centrelines of 11 —
 * the caps add half a stroke at each end, so the ink measures 12.5px across.
 *
 * `currentColor` keeps colour with the button's own tokens
 * (`toolbarIconButton`, plus its open-state tint) — this glyph owns size only.
 */
function PlusGlyph() {
  return (
    <svg
      width={16}
      height={16}
      viewBox="0 0 16 16"
      fill="none"
      stroke="currentColor"
      strokeWidth={1.5}
      strokeLinecap="round"
      aria-hidden="true"
    >
      <path d="M2.5 8h11" />
      <path d="M8 2.5v11" />
    </svg>
  );
}

export function ComposerPlusMenu({
  readOnly,
  onPickAttachments,
  disabled,
  agentId,
  sessionId,
  showSkills = true,
  placement = "top",
  buttonClassName,
  openClassName,
  portal = false,
}: {
  /** ADR-076: a session shared with us is read-only — attachments are
   *  session-write input and tool switches are session writes, so the whole
   *  trigger is disabled rather than half-working. */
  readOnly?: boolean;
  /** Open the OS file picker (the handler the removed paperclip used). */
  onPickAttachments: () => void;
  /** No session / agent yet. */
  disabled?: boolean;
  /** Target agent (ADR-073 instance id). Absent = the surface's selected
   *  agent. The Chat execution block passes the block's own agent, because
   *  the global selection points at the Chat surface, not at the run. */
  agentId?: string | null;
  /** Target session. Absent = the agent's active session. */
  sessionId?: string | null;
  /** The Skills row rides the exec composer's pending-send state (`activeSkill`
   *  is that input's, not the session's), so a surface without that input —
   *  the block's reply drawer — hides it instead of writing state nothing reads. */
  showSkills?: boolean;
  /** Which side the panel opens on. The composer strip opens upward ("top");
   *  the block's action row sits above its transcript, so it opens downward. */
  placement?: "top" | "bottom";
  /** Trigger style override — the block's action row uses its own 24px round
   *  cluster instead of `toolbarIconButton`. */
  buttonClassName?: string;
  /** Trigger style while the menu is open (mirrors `buttonClassName` for the
   *  open tint, which is surface-specific). */
  openClassName?: string;
  /** Render the panel through a `document.body` portal with `position: fixed`
   *  placement. The Chat block's row lives inside a scroll container that
   *  hard-crops an absolutely-positioned panel (the failure
   *  `WorkspaceSelector`/`ContextUsageIcon` document); a surface inside one
   *  sets this. */
  portal?: boolean;
}) {
  const { t } = useTranslation();
  // Selector form, not a bare destructure: the bare call subscribes to the
  // whole store (every agent mutation re-renders the menu) and assumes the
  // hook is always invoked with a selector.
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const activeSessionId = useChatStore((s) =>
    selectedAgentId ? (s.agentStates[selectedAgentId]?.activeSessionId ?? null) : null,
  );
  // The override-or-global pair: every read/write below targets THIS menu's
  // session, which on the exec composer is the global selection (props absent)
  // and on a Chat execution block is the block's own run (props present).
  const targetAgentId = agentId ?? selectedAgentId;
  const targetSessionId = sessionId ?? activeSessionId;

  const [open, setOpen] = useState(false);
  const [view, setView] = useState<PlusView>({ kind: "root" });
  const ref = useRef<HTMLDivElement>(null);
  // Portal-mode panel: measured placement, same grammar as
  // `WorkspaceSelector` / the block's `ContextUsageIcon` — the panel must
  // escape the transcript's scroll container, which hard-crops it otherwise.
  const panelRef = useRef<HTMLDivElement>(null);
  const [panelPos, setPanelPos] = useState<{ top: number; left: number } | null>(null);

  const loadInventory = useToolPolicyStore((s) => s.loadInventory);
  const loadPolicy = useToolPolicyStore((s) => s.loadPolicy);
  const toggleBuiltin = useToolPolicyStore((s) => s.toggleBuiltin);
  const toggleMcpServer = useToolPolicyStore((s) => s.toggleMcpServer);
  const toggleMcpTool = useToolPolicyStore((s) => s.toggleMcpTool);
  const resetGroup = useToolPolicyStore((s) => s.resetGroup);

  const inventory: AgentToolInventory | undefined = useToolPolicyStore((s) =>
    targetAgentId ? s.inventory[targetAgentId] : undefined,
  );
  const inventoryLoading = useToolPolicyStore((s) =>
    targetAgentId ? (s.inventoryLoading[targetAgentId] ?? false) : false,
  );
  const inventoryError = useToolPolicyStore((s) =>
    targetAgentId ? (s.inventoryError[targetAgentId] ?? null) : null,
  );
  // `selectPolicyEntry` returns a module-level constant when the session has
  // no entry yet, so this selector is identity-stable across renders.
  const policyEntry = useToolPolicyStore((s) =>
    selectPolicyEntry(s, targetAgentId, targetSessionId),
  );
  const policy: SessionToolPolicy = policyEntry.policy;

  // Both layers are re-read on every open; flipping between levels costs no
  // further reads because the store caches them for the duration of the visit.
  //
  // The agent layer is refreshed every time (not once per session) because the
  // settings panel is not its only writer: MCP tools appear only after a
  // server's connection completes, so a copy cached at the first open would
  // list a server with no tools — and hence no drill-down — for the rest of the
  // session, with no way for the user to refresh it. The store keeps the
  // previous copy visible while the refresh is in flight, so this costs no
  // flicker.
  useEffect(() => {
    if (!open || !targetAgentId || !targetSessionId) return;
    void loadInventory(targetAgentId, { force: true });
    void loadPolicy(targetAgentId, targetSessionId, { force: true });
  }, [open, targetAgentId, targetSessionId, loadInventory, loadPolicy]);

  // Closing resets the depth, so reopening always starts at level 1.
  useEffect(() => {
    if (!open) setView({ kind: "root" });
  }, [open]);

  // ── Portal-mode placement ─────────────────────────────────────────
  const computePanelPos = useCallback(() => {
    const anchorEl = ref.current;
    const panelEl = panelRef.current;
    if (!anchorEl || !panelEl) return;
    const r = anchorEl.getBoundingClientRect();
    const ph = panelEl.offsetHeight;
    // Prefer the requested side; flip only when it cannot fit there but
    // fits on the other. (The block asks for "bottom".)
    const fitsRequested =
      (placement === "bottom"
        ? window.innerHeight - r.bottom >= ph + DROPDOWN_EDGE_GAP
        : r.top >= ph + DROPDOWN_EDGE_GAP);
    const flipFits =
      placement === "bottom"
        ? r.top >= ph + DROPDOWN_EDGE_GAP
        : window.innerHeight - r.bottom >= ph + DROPDOWN_EDGE_GAP;
    const direction = fitsRequested || !flipFits
      ? (placement === "bottom" ? "down" : "up")
      : (placement === "bottom" ? "up" : "down");
    const p = computeDropdownPlacement({
      anchor: { left: r.left, right: r.right, top: r.top, bottom: r.bottom },
      menuWidth: panelEl.offsetWidth,
      menuHeight: ph,
      viewportWidth: window.innerWidth,
      viewportHeight: window.innerHeight,
      direction,
      // Same rule as the block's usage popover: the row sits on the chat
      // pane's LEFT side, so the panel grows rightward, back into the chat.
      align: "left",
    });
    setPanelPos({ top: p.top, left: p.left });
  }, [placement]);

  // Measure synchronously before paint so the panel never flashes at (0,0).
  useLayoutEffect(() => {
    if (!portal) return;
    if (open) computePanelPos();
    else setPanelPos(null);
  }, [open, portal, computePanelPos]);

  // Re-measure while open: drill-down swaps the panel's height, the
  // transcript scrolls under it, the window resizes.
  useEffect(() => {
    if (!portal || !open) return;
    computePanelPos();
    const panelEl = panelRef.current;
    const ro =
      typeof ResizeObserver !== "undefined" && panelEl
        ? new ResizeObserver(() => computePanelPos())
        : null;
    if (ro && panelEl) ro.observe(panelEl);
    window.addEventListener("resize", computePanelPos);
    window.addEventListener("scroll", computePanelPos, true);
    return () => {
      ro?.disconnect();
      window.removeEventListener("resize", computePanelPos);
      window.removeEventListener("scroll", computePanelPos, true);
    };
  }, [portal, open, computePanelPos]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      const target = e.target as Node;
      // The portalled panel is NOT a descendant of the trigger wrapper —
      // testing only `ref` would close the menu on the first click inside
      // a switch (the trap `WorkspaceSelector` documents).
      if (ref.current?.contains(target) || panelRef.current?.contains(target)) return;
      setOpen(false);
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key !== "Escape") return;
      // Escape steps back one level before it closes — the menu is a stack.
      setView((v) => (v.kind === "root" ? v : { kind: "root" }));
      if (view.kind === "root") setOpen(false);
    };
    document.addEventListener("mousedown", onDown);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("mousedown", onDown);
      document.removeEventListener("keydown", onKey);
    };
  }, [open, view.kind]);

  const blocked = disabled || readOnly;
  // A switch is a *whole-policy replacement*, so it must not be offered before
  // the read that establishes the current value has landed — writing an empty
  // base would erase the denies set by another surface. `policyReady` is the
  // same condition the store enforces; the UI just refuses to lie about it.
  const policyReady = policyEntry.loaded;
  const rowBusy = policyEntry.saving || !policyReady;

  const counts = useMemo(() => {
    const builtinOn = inventory
      ? inventory.builtin.filter((x) => deriveBuiltinState(x, policy).effective)
          .length
      : 0;
    const builtinTotal = inventory?.builtin.length ?? 0;
    const serversOn = inventory
      ? inventory.mcpServers.filter((s) => deriveMcpServerState(s, policy).effective)
          .length
      : 0;
    const serversTotal = inventory?.mcpServers.length ?? 0;
    return { builtinOn, builtinTotal, serversOn, serversTotal };
  }, [inventory, policy]);

  const activeServer =
    view.kind === "mcpTools"
      ? inventory?.mcpServers.find((s) => s.name === view.server)
      : undefined;

  return (
    // `data-toolbar-btn` (no `data-toolbar-text` / `-chevron`): the responsive
    // fold engine collects every `[data-toolbar-btn]` to size the row, so an
    // unmarked icon button is invisible to it and the row would over-subscribe
    // the available width. Marked with no label, its fold delta is 0 — it
    // contributes width without ever being foldable, which is what an icon-only
    // trigger is.
    <div ref={ref} data-toolbar-btn="plus" className="relative inline-block shrink-0">
      <Tooltip
        content={
          readOnly
            ? t("chatPanel.readOnlySession")
            : disabled
              ? t("chatPanel.plusMenuUnavailable")
              : t("chatPanel.plusMenu")
        }
      >
        <button
          type="button"
          onClick={() => setOpen((v) => !v)}
          disabled={blocked}
          aria-label={t("chatPanel.plusMenu")}
          aria-expanded={open}
          className={cn(
            buttonClassName ?? toolbarIconButton,
            open && (openClassName ?? "bg-zinc-200 text-text dark:bg-zinc-700"),
          )}
        >
          <PlusGlyph />
        </button>
      </Tooltip>

      {open && (() => {
        const panel = (
        <div
          ref={portal ? panelRef : undefined}
          className={cn(
            "z-[100] flex w-72 flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-lg",
            portal
              ? panelPos
                ? "visible"
                : "invisible"
              : cn(
                  "absolute left-0",
                  placement === "top" ? "bottom-full mb-1" : "top-full mt-1",
                ),
          )}
          style={
            portal
              ? {
                  // Fixed + portalled: escapes the transcript's scroll
                  // container, which hard-crops an absolute panel.
                  position: "fixed",
                  top: panelPos?.top ?? 0,
                  left: panelPos?.left ?? 0,
                  maxHeight: "min(380px, 60vh)",
                }
              : { maxHeight: "min(380px, 60vh)" }
          }
          role="menu"
        >
          {view.kind === "root" ? (
            <div className="py-1">
              {showSkills && (
                <RootRow
                  icon={<Sparkles className="h-3.5 w-3.5" />}
                  label={t("chatPanel.plusSkills")}
                  onClick={() => setView({ kind: "skills" })}
                />
              )}
              <RootRow
                icon={<Plug className="h-3.5 w-3.5" />}
                label={t("chatPanel.plusMcp")}
                value={
                  inventory ? `${counts.serversOn}/${counts.serversTotal}` : undefined
                }
                onClick={() => setView({ kind: "mcp" })}
              />
              <RootRow
                icon={<Wrench className="h-3.5 w-3.5" />}
                label={t("chatPanel.plusBuiltinTools")}
                value={
                  inventory ? `${counts.builtinOn}/${counts.builtinTotal}` : undefined
                }
                onClick={() => setView({ kind: "builtin" })}
              />
              <RootRow
                icon={<Paperclip className="h-3.5 w-3.5" />}
                label={t("chatPanel.plusAttachments")}
                onClick={() => {
                  setOpen(false);
                  onPickAttachments();
                }}
              />
            </div>
          ) : (
            <>
              <MenuHeader
                title={
                  view.kind === "skills"
                    ? t("chatPanel.plusSkills")
                    : view.kind === "builtin"
                      ? t("chatPanel.plusBuiltinTools")
                      : view.kind === "mcp"
                        ? t("chatPanel.plusMcp")
                        : (activeServer?.displayName?.trim() || activeServer?.name || view.server)
                }
                onBack={() => setView({ kind: "root" })}
                onReset={
                  view.kind === "builtin"
                    ? () => targetAgentId && targetSessionId &&
                        void resetGroup(targetAgentId, targetSessionId, "builtin")
                    : view.kind === "mcp"
                      ? () => targetAgentId && targetSessionId &&
                          void resetGroup(targetAgentId, targetSessionId, "mcpServers")
                      : view.kind === "mcpTools"
                        ? () => targetAgentId && targetSessionId &&
                            void resetGroup(targetAgentId, targetSessionId, "mcpTools")
                        : undefined
                }
                resetDisabled={
                  !policyReady ||
                  (view.kind === "builtin"
                    ? policy.disabled_builtin_tools.length === 0
                    : view.kind === "mcp"
                      ? policy.disabled_mcp_servers.length === 0
                      : view.kind === "mcpTools"
                        ? policy.disabled_mcp_tools.length === 0
                        : true)
                }
                resetTip={t("chatPanel.followAgentDefault")}
                saving={policyEntry.saving || policyEntry.loading}
              />

              {/* The menu body is the only part that can overflow (the panel is
                  capped at `min(380px, 60vh)`), so it gets the app's floating
                  overlay thumb instead of an OS scrollbar — it is strictly
                  inside the panel's `overflow-hidden` rounded frame. */}
              <OverlayScrollbar fill="flex" className="px-1.5 pb-1.5">
                {/* The policy groups' switches stay disabled until the session
                    layer has been read — say so instead of leaving a wall of
                    dead switches unexplained. */}
                {(view.kind === "builtin" ||
                  view.kind === "mcp" ||
                  view.kind === "mcpTools") &&
                  inventory !== undefined &&
                  !policyReady && <BusyLine label={t("chatPanel.plusLoading")} />}

                {view.kind === "skills" && (
                  <SkillMenuList
                    readOnly={readOnly}
                    onDone={() => setOpen(false)}
                  />
                )}

                {(view.kind === "builtin" || view.kind === "mcp") &&
                  !inventory &&
                  (inventoryLoading ? (
                    <BusyLine label={t("chatPanel.plusLoading")} />
                  ) : (
                    <HintLine
                      text={
                        inventoryError ?? t("chatPanel.plusLoadFailed")
                      }
                    />
                  ))}

                {view.kind === "builtin" && inventory && (
                  <>
                    {inventory.builtin.length === 0 ? (
                      <HintLine text={t("chatPanel.plusNoTools")} />
                    ) : (
                      inventory.builtin.map((tool) => {
                        const st = deriveBuiltinState(tool, policy);
                        return (
                          <SwitchRow
                            key={tool.name}
                            label={tool.name}
                            state={st}
                            saving={rowBusy}
                            onChange={(on) =>
                              targetAgentId &&
                              targetSessionId &&
                              void toggleBuiltin(
                                targetAgentId,
                                targetSessionId,
                                tool.name,
                                on,
                              )
                            }
                          />
                        );
                      })
                    )}
                    <ResetNote
                      agentDisabledCount={
                        inventory.builtin.filter((x) => !x.agentEnabled).length
                      }
                    />
                  </>
                )}

                {view.kind === "mcp" && inventory && (
                  <>
                    {inventory.mcpServers.length === 0 ? (
                      <HintLine text={t("chatPanel.plusNoMcp")} />
                    ) : (
                      inventory.mcpServers.map((server) => {
                        const st = deriveMcpServerState(server, policy);
                        return (
                          <SwitchRow
                            key={server.name}
                            label={server.displayName?.trim() || server.name}
                            sub={
                              server.tools.length > 0
                                ? t("chatPanel.plusToolCount", {
                                    n: server.tools.length,
                                  })
                                : t("chatPanel.plusNoToolsConnected")
                            }
                            state={st}
                            saving={rowBusy}
                            // A server switched off by the agent has no tool
                            // list to drill into — the chevron would open an
                            // empty box.
                            drill={
                              server.agentEnabled && server.tools.length > 0
                                ? () =>
                                    setView({
                                      kind: "mcpTools",
                                      server: server.name,
                                    })
                                : undefined
                            }
                            onChange={(on) =>
                              targetAgentId &&
                              targetSessionId &&
                              void toggleMcpServer(
                                targetAgentId,
                                targetSessionId,
                                server.name,
                                on,
                              )
                            }
                          />
                        );
                      })
                    )}
                  </>
                )}

                {view.kind === "mcpTools" && activeServer && (
                  <>
                    {activeServer.tools.map((tool) => {
                      const st = deriveMcpToolState(tool, policy);
                      return (
                        <SwitchRow
                          key={tool.tool}
                          label={tool.tool}
                          sub={tool.description ?? undefined}
                          state={st}
                          saving={rowBusy}
                          onChange={(on) =>
                            targetAgentId &&
                            targetSessionId &&
                            void toggleMcpTool(
                              targetAgentId,
                              targetSessionId,
                              tool.server,
                              tool.tool,
                              on,
                            )
                          }
                        />
                      );
                    })}
                  </>
                )}
              </OverlayScrollbar>

              {policyEntry.error && (
                <HintLine text={policyEntry.error} tone="error" />
              )}
            </>
          )}
        </div>
        );
        return portal ? createPortal(panel, document.body) : panel;
      })()}
    </div>
  );
}

/** A level-1 row: icon + label + optional count + chevron. */
function RootRow({
  icon,
  label,
  value,
  onClick,
}: {
  icon: React.ReactNode;
  label: string;
  value?: string;
  onClick: () => void;
}) {
  return (
    <button
      type="button"
      role="menuitem"
      onClick={onClick}
      className={cn(
        "flex w-full items-center gap-2 rounded px-2 py-1.5 text-left text-xs text-text-secondary transition-colors",
        menuItemHover,
      )}
    >
      <span className="shrink-0 text-text-tertiary">{icon}</span>
      <span className="min-w-0 flex-1 truncate">{label}</span>
      {value && (
        <span className="shrink-0 text-10 text-text-tertiary">{value}</span>
      )}
      <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
    </button>
  );
}

/** Back chevron + title + per-group "follow the agent" reset. */
function MenuHeader({
  title,
  onBack,
  onReset,
  resetDisabled,
  resetTip,
  saving,
}: {
  title: string;
  onBack: () => void;
  onReset?: () => void;
  resetDisabled?: boolean;
  resetTip: string;
  saving?: boolean;
}) {
  return (
    <div className="flex items-center gap-1 border-b border-border-divider px-1.5 py-1.5">
      <button
        type="button"
        onClick={onBack}
        aria-label="back"
        className={cn(toolbarIconButton, "h-6 w-6")}
      >
        <ChevronLeft size={14} />
      </button>
      <span className="min-w-0 flex-1 truncate text-xs font-medium text-text-secondary">
        {title}
      </span>
      {saving && (
        <Loader2 className="h-3 w-3 shrink-0 animate-spin text-text-tertiary" />
      )}
      {onReset && (
        <Tooltip content={resetTip}>
          <button
            type="button"
            onClick={onReset}
            disabled={resetDisabled}
            aria-label={resetTip}
            className={cn(toolbarIconButton, "h-6 w-6")}
          >
            <RotateCcw size={13} />
          </button>
        </Tooltip>
      )}
    </div>
  );
}

/**
 * One switchable row. Not a `<button>`: `Switch` renders its own `<label>`, and
 * nesting a label inside a button is invalid HTML and breaks the click target.
 *
 * When the row drills, the row *is* the target — `role="button"` on the div,
 * which is exactly how `ExpandableRow` does it. The first version had this
 * backwards: the drill was a separate 24px chevron button inside a ~260px row,
 * while the row itself carried a hover tint that promised a click would do
 * something. Clicking the row — what the settings panel trains users to do, and
 * what the hover says — did nothing at all. The chevron now stays what it was
 * always better at: an `aria-hidden` indicator of depth.
 *
 * The `Switch` keeps its own click and stops propagation, so flipping a server
 * on or off does not also navigate into its tools — the same contract as
 * `ExpandableRow`'s `trailing` slot.
 */
function SwitchRow({
  label,
  sub,
  state,
  saving,
  onChange,
  drill,
}: {
  label: string;
  sub?: string;
  state: { agentEnabled: boolean; denied: boolean; effective: boolean };
  saving?: boolean;
  onChange: (on: boolean) => void;
  drill?: () => void;
}) {
  const { t } = useTranslation();
  const lockedByAgent = !state.agentEnabled;
  const drillable = !!drill;
  return (
    <div
      role={drillable ? "button" : undefined}
      tabIndex={drillable ? 0 : undefined}
      aria-label={drillable ? t("chatPanel.plusShowTools") : undefined}
      onClick={drillable ? drill : undefined}
      onKeyDown={
        drillable
          ? (e) => {
              if (e.key === "Enter" || e.key === " ") {
                e.preventDefault();
                drill();
              }
            }
          : undefined
      }
      className={cn(
        "flex items-center gap-1.5 rounded px-1.5 py-1",
        // The hover tint is a promise that clicking the row does something, so
        // it may only sit on a row that actually does one.
        drillable && [menuItemHover, "cursor-pointer"],
      )}
    >
      <div className="min-w-0 flex-1">
        <div
          className={cn(
            "truncate text-xs",
            lockedByAgent ? "text-text-tertiary" : "text-text-secondary",
            state.denied && "line-through",
          )}
        >
          {label}
        </div>
        {sub && (
          <div className="truncate text-10 text-text-tertiary">{sub}</div>
        )}
      </div>
      {drillable && (
        <ChevronRight
          aria-hidden
          className="h-3.5 w-3.5 shrink-0 text-text-tertiary"
        />
      )}
      <span onClick={(e) => e.stopPropagation()}>
        <Switch
          size="sm"
          checked={state.effective}
          disabled={lockedByAgent || saving}
          onChange={onChange}
        />
      </span>
    </div>
  );
}

function BusyLine({ label }: { label: string }) {
  return (
    <div className="flex items-center justify-center gap-1.5 px-2 py-3 text-xs text-text-tertiary">
      <Loader2 className="h-3 w-3 animate-spin" />
      {label}
    </div>
  );
}

function HintLine({ text, tone }: { text: string; tone?: "error" }) {
  return (
    <div
      className={cn(
        "px-2 py-3 text-center text-xs",
        tone === "error" ? "text-red-500 dark:text-red-400" : "text-text-tertiary",
      )}
    >
      {text}
    </div>
  );
}

/** Explains the disabled switches instead of leaving them unexplained. */
function ResetNote({ agentDisabledCount }: { agentDisabledCount: number }) {
  const { t } = useTranslation();
  if (agentDisabledCount === 0) return null;
  return (
    <div className="mt-1 px-1.5 pb-0.5 text-10 text-text-tertiary">
      {t("chatPanel.plusAgentDisabledNote", { n: agentDisabledCount })}
    </div>
  );
}
