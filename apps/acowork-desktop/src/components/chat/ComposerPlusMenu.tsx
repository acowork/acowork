import { useEffect, useMemo, useRef, useState } from "react";
import {
  Plus,
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
import { menuItemHover, toolbarIconButton } from "../../lib/ui-styles";
import { Tooltip } from "../common/Tooltip";
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

export function ComposerPlusMenu({
  readOnly,
  onPickAttachments,
  disabled,
}: {
  /** ADR-076: a session shared with us is read-only — attachments are
   *  session-write input and tool switches are session writes, so the whole
   *  trigger is disabled rather than half-working. */
  readOnly?: boolean;
  /** Open the OS file picker (the handler the removed paperclip used). */
  onPickAttachments: () => void;
  /** No session / agent yet. */
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const { selectedAgentId } = useAgentStore();
  const currentSessionId = useChatStore((s) =>
    selectedAgentId ? (s.agentStates[selectedAgentId]?.activeSessionId ?? null) : null,
  );

  const [open, setOpen] = useState(false);
  const [view, setView] = useState<PlusView>({ kind: "root" });
  const ref = useRef<HTMLDivElement>(null);

  const loadInventory = useToolPolicyStore((s) => s.loadInventory);
  const loadPolicy = useToolPolicyStore((s) => s.loadPolicy);
  const toggleBuiltin = useToolPolicyStore((s) => s.toggleBuiltin);
  const toggleMcpServer = useToolPolicyStore((s) => s.toggleMcpServer);
  const toggleMcpTool = useToolPolicyStore((s) => s.toggleMcpTool);
  const resetGroup = useToolPolicyStore((s) => s.resetGroup);

  const inventory: AgentToolInventory | undefined = useToolPolicyStore((s) =>
    selectedAgentId ? s.inventory[selectedAgentId] : undefined,
  );
  const inventoryLoading = useToolPolicyStore((s) =>
    selectedAgentId ? (s.inventoryLoading[selectedAgentId] ?? false) : false,
  );
  const inventoryError = useToolPolicyStore((s) =>
    selectedAgentId ? (s.inventoryError[selectedAgentId] ?? null) : null,
  );
  // `selectPolicyEntry` returns a module-level constant when the session has
  // no entry yet, so this selector is identity-stable across renders.
  const policyEntry = useToolPolicyStore((s) =>
    selectPolicyEntry(s, selectedAgentId, currentSessionId),
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
    if (!open || !selectedAgentId || !currentSessionId) return;
    void loadInventory(selectedAgentId, { force: true });
    void loadPolicy(selectedAgentId, currentSessionId, { force: true });
  }, [open, selectedAgentId, currentSessionId, loadInventory, loadPolicy]);

  // Closing resets the depth, so reopening always starts at level 1.
  useEffect(() => {
    if (!open) setView({ kind: "root" });
  }, [open]);

  useEffect(() => {
    if (!open) return;
    const onDown = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) setOpen(false);
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
            toolbarIconButton,
            open && "bg-zinc-200 text-text dark:bg-zinc-700",
          )}
        >
          <Plus size={14} />
        </button>
      </Tooltip>

      {open && (
        <div
          className="absolute bottom-full left-0 z-[100] mb-1 flex w-72 flex-col overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-lg"
          style={{ maxHeight: "min(380px, 60vh)" }}
          role="menu"
        >
          {view.kind === "root" ? (
            <div className="py-1">
              <RootRow
                icon={<Sparkles className="h-3.5 w-3.5" />}
                label={t("chatPanel.plusSkills")}
                onClick={() => setView({ kind: "skills" })}
              />
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
                    ? () => selectedAgentId && currentSessionId &&
                        void resetGroup(selectedAgentId, currentSessionId, "builtin")
                    : view.kind === "mcp"
                      ? () => selectedAgentId && currentSessionId &&
                          void resetGroup(selectedAgentId, currentSessionId, "mcpServers")
                      : view.kind === "mcpTools"
                        ? () => selectedAgentId && currentSessionId &&
                            void resetGroup(selectedAgentId, currentSessionId, "mcpTools")
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

              <div className="min-h-0 flex-1 overflow-y-auto px-1.5 pb-1.5">
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
                              selectedAgentId &&
                              currentSessionId &&
                              void toggleBuiltin(
                                selectedAgentId,
                                currentSessionId,
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
                              selectedAgentId &&
                              currentSessionId &&
                              void toggleMcpServer(
                                selectedAgentId,
                                currentSessionId,
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
                            selectedAgentId &&
                            currentSessionId &&
                            void toggleMcpTool(
                              selectedAgentId,
                              currentSessionId,
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
              </div>

              {policyEntry.error && (
                <HintLine text={policyEntry.error} tone="error" />
              )}
            </>
          )}
        </div>
      )}
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
  const lockedByAgent = !state.agentEnabled;
  return (
    <div
      className={cn(
        "flex items-center gap-1.5 rounded px-1.5 py-1",
        menuItemHover,
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
      {drill && (
        <button
          type="button"
          onClick={drill}
          aria-label="tools"
          className={cn(toolbarIconButton, "h-6 w-6")}
        >
          <ChevronRight size={13} />
        </button>
      )}
      <Switch
        size="sm"
        checked={state.effective}
        disabled={lockedByAgent || saving}
        onChange={onChange}
      />
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
