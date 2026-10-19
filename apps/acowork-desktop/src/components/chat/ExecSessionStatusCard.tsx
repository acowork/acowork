// Design 27 §5.5.5 / §6: the Chat surface's execution-status card.
//
// The in-flight turn itself is projected at read time from the Chat session
// rows (chatSessionStore.inFlight → { triggerId, execSid }) — that pointer is
// all the durable surface needs. Everything LIVE shown here is a mirror of the
// backend event stream that ALREADY lands in chatStore.sessionStates[execSid]
// for that execution session, whether or not its tab is open: status, the
// in-flight tool / approval / error / pause, the resolved model/provider and
// the workspace. The Chat surface renders that mirror and forwards every
// control action to the session-scoped HTTP endpoints (ADR-076) — it owns no
// independent state or business logic of its own. That is the whole design
// principle: the backend is the single source of truth, the frontend shows it
// and provides the entry point to act on it.
//
// Visual language is the global card grammar (the right-panel Session Status /
// Agent Status cards, settings/harness): `rounded-md border border-border-outer
// bg-panel-block` shell, `bg-panel-inset` sub-blocks, `StatRow` label/value
// lines, the ADR-049 six-colour state dot. The card grows linearly downward:
// the detail rows are fixed, the action region below appends one block per
// pending error / approval / pause as they arrive.

import { useEffect, useMemo, useRef, useState, type ReactNode } from "react";
import { ExternalLink } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { getProcessingPhase } from "../../lib/types";
import type { ToolApprovalNeededEvent, TodoItem } from "../../lib/types";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import * as sessionControl from "../../lib/session-control";
import { log } from "../../lib/logger";
import { AgentAvatar } from "../common/AgentAvatar";

interface ExecSessionStatusCardProps {
  agentId: string;
  /** The execution session the in-flight turn is bound to. `null` = the chat
   *  message is delivered but the Runtime has not yet resolved (or created)
   *  the execution session — the queued state, no live detail to mirror. */
  execSid: string | null;
  /** Drill into the execution view (open the exec tab + flip the surface). */
  onDrill?: () => void;
}

/** `alias ?? path basename`, else "Agent Home" for the sentinel — same rule
 *  the WorkspaceSelector uses, so the card and the toolbar never disagree. */
function workspaceDisplayName(id: string | undefined, alias: string | null | undefined, path: string | undefined, agentHomeLabel: string): string {
  if (!id || id === "__agent_home__") return agentHomeLabel;
  const base = (alias?.trim() || path || id).split(/[\\/]+/).filter(Boolean).pop();
  return base || id;
}

export function ExecSessionStatusCard({ agentId, execSid, onDrill }: ExecSessionStatusCardProps) {
  const { t } = useTranslation();
  const agent = useAgentStore((s) => s.agents[agentId]?.meta);
  const agentName = agent?.display_name ?? agent?.name ?? "";

  // ── live mirror of the execution session (keyed by execSid, not the
  //    foreground session — a chat-driven turn usually has no open tab) ──
  const sessionStatus = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.sessionStatus ?? null : null,
  );
  const model = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.model ?? null : null,
  );
  const provider = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.provider ?? null : null,
  );
  const todos = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.todos ?? [] : [],
  );
  const toolProgress = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.toolProgress ?? {} : {},
  );
  const pendingApproval = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.pendingApproval ?? {} : {},
  );
  const serverError = useChatStore((s) =>
    execSid ? s.agentStates[agentId]?.sessionStates[execSid]?.serverError ?? null : null,
  );
  const title = useAgentStore((s) =>
    execSid ? s.agents[agentId]?.sessions.find((x) => x.session_id === execSid)?.title ?? null : null,
  );
  const workspaceName = useWorkspaceStore((s) => {
    if (!execSid) return null;
    const id = s.sessionWorkspaceMap[execSid];
    if (!id || id === "__agent_home__") return null; // rendered via sentinel below
    const dir = s.workspaces.find((w) => w.id === id);
    return workspaceDisplayName(id, dir?.alias, dir?.path, t("chatView.statusCard.workspaceAgentHome"));
  });

  // The exec session's model/provider/workspace/title are authoritative only
  // on the Runtime's config snapshot. The chat turn's session usually has no
  // open tab, so nothing else pulls it — fetch it once per turn so the mirror
  // has real values instead of empty rows. Guarded per execSid.
  const fetchedRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    if (!execSid || fetchedRef.current.has(execSid)) return;
    fetchedRef.current.add(execSid);
    void useChatStore.getState().fetchSessionConfig(agentId, execSid);
  }, [agentId, execSid]);

  // ── derived: coarse phase → colour + label; current step ──
  const phase = getProcessingPhase(sessionStatus);
  const statusLabel = sessionStatus
    ? t(`chatView.statusCard.status.${sessionStatus.status}`)
    : "\u2014";

  const approvals = useMemo(
    () => Object.values(pendingApproval) as ToolApprovalNeededEvent[],
    [pendingApproval],
  );
  const runningToolCount = Object.keys(toolProgress).length;
  const activeTodoIndex = useMemo(
    () => (todos as TodoItem[]).findIndex((x) => x.status === "in_progress"),
    [todos],
  );
  const activeTodo =
    activeTodoIndex >= 0 ? (todos as TodoItem[])[activeTodoIndex] : null;

  // Step priority: an approval waiting on the user first (it is the reason the
  // run is stalled), then an in-flight tool, then the active todo — annotated
  // with its position so "step 2 of 5" is legible — then a phase fallback.
  const step = useMemo(() => {
    if (approvals.length > 0) {
      return t("chatView.statusCard.stepApproval", { tool: approvals[0].tool_name });
    }
    if (runningToolCount > 0) {
      return t("chatView.statusCard.stepRunningTool", { count: runningToolCount });
    }
    if (activeTodo) {
      return t("chatView.statusCard.stepTodo", {
        content: activeTodo.content,
        index: activeTodoIndex + 1,
        total: (todos as TodoItem[]).length,
      });
    }
    return t(`chatView.statusCard.phase.${phase}`);
  }, [approvals, runningToolCount, activeTodo, activeTodoIndex, todos, phase, t]);

  // Pause that needs a human decision (iteration limit / loop) — the 429 retry
  // wait (`retry_info`) and DevMode (`reason: "debug"`) resume on their own, so
  // only these two show the Continue action, matching the exec view's banner.
  const pausedDetail = sessionStatus?.status === "paused" ? sessionStatus.detail ?? null : null;
  const needsContinue =
    pausedDetail?.reason === "iteration_limit" || pausedDetail?.reason === "loop_detected";

  const dotClass = cn(
    "inline-block h-2 w-2 shrink-0 rounded-full",
    phase === "streaming" && "bg-[var(--color-accent)] animate-pulse",
    phase === "thinking" && "bg-[var(--color-accent)] animate-pulse",
    phase === "waiting" && "bg-zinc-400 dark:bg-zinc-500",
    phase === "tool_executing" && "bg-blue-400",
    phase === "waiting_approval" && "bg-yellow-400",
    phase === "paused" && "bg-amber-400",
    (phase === "idle" || !sessionStatus) && "bg-zinc-300 dark:bg-zinc-600",
  );

  // ── actions (all forwarded to session-scoped HTTP; store cache cleared
  //    against execSid explicitly, never the foreground session) ──
  const [busyKey, setBusyKey] = useState<string | null>(null);

  const decideApproval = async (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => {
    const key = approval.tool_call_id || approval.request_id;
    setBusyKey(key);
    try {
      await sessionControl.sendApproval(agentId, execSid!, {
        request_id: approval.request_id,
        approved: action === "allow",
        allow_all_session: false,
        reason: "",
      });
      // Drop it from the local mirror so the button disappears immediately;
      // the backend `tool_approval_resolved` event is the durable confirmation.
      if (approval.tool_call_id) {
        useChatStore.getState().resolveApprovalByToolCallId(agentId, approval.tool_call_id, execSid!);
      }
    } catch (err) {
      log.error("[ExecSessionStatusCard] approval failed:", err);
    } finally {
      setBusyKey(null);
    }
  };

  const doContinue = async () => {
    if (!execSid) return;
    setBusyKey("continue");
    try {
      await sessionControl.continueSession(agentId, execSid);
    } catch (err) {
      log.error("[ExecSessionStatusCard] continue failed:", err);
    } finally {
      setBusyKey(null);
    }
  };

  const dismissError = () => {
    if (!execSid) return;
    useChatStore.getState().clearServerError(agentId, execSid);
  };

  // The card lives in the Chat stream exactly like an agent message: an
  // avatar + name header, then the card indented into the message column
  // (past the avatar gutter) — never a lone left-aligned block. The column is
  // capped (`max-w-md`) so the label/value rows stay compact.
  const shell = (children: ReactNode) => (
    <div className="px-6 py-1">
      <div className="mb-2 mt-1 flex items-center gap-2">
        <AgentAvatar
          agentId={agentId}
          displayName={agentName}
          avatarUrl={agent?.avatar}
          version={agent?.version}
          builtinAvatarId={agent?.builtin_avatar ?? null}
          size={40}
          className="shrink-0"
        />
        <div className="flex flex-col">
          <span className="text-xs font-medium text-text-tertiary">{agentName}</span>
          {agent?.role && (
            <span className="text-10 leading-tight text-text-tertiary">{agent.role}</span>
          )}
        </div>
      </div>
      <div className="flex items-start gap-2">
        <div className="w-10 shrink-0" />
        <div className="min-w-0 max-w-md">{children}</div>
      </div>
    </div>
  );

  // ── queued (no exec session resolved yet): the minimal live row, the only
  //    state the Chat surface can honestly show before the Runtime answers ──
  if (!execSid) {
    return shell(
      <div className="flex items-center gap-2 rounded-md border border-border-outer bg-panel-block px-3 py-2 text-xs">
        <span className="h-2 w-2 shrink-0 animate-pulse rounded-full bg-zinc-400 dark:bg-zinc-500" />
        <span className="min-w-0 flex-1 truncate text-text-secondary">{t("chatView.queued")}</span>
      </div>
    );
  }

  const hasActions =
    !!serverError || approvals.length > 0 || needsContinue || !!pausedDetail?.message;

  return shell(
    <div className="rounded-md border border-border-outer bg-panel-block text-xs">
      {/* Header — status dot + coarse status on the LEFT; an explicit "详情"
          button on the RIGHT drills into the execution session. The trigger is
          a labeled button, not the whole bar, so what is clickable is obvious.
          No title here — it lives in the detail rows below (no duplication). */}
      <div className="flex items-center gap-2 px-3 py-2">
        <span className={dotClass} />
        <span className="shrink-0 font-medium text-text-secondary">{statusLabel}</span>
        <span className="min-w-0 flex-1" />
        {onDrill && (
          <button
            type="button"
            onClick={onDrill}
            className="inline-flex shrink-0 items-center gap-1 rounded-md border border-border-outer px-2 py-0.5 text-text-tertiary transition-colors hover:bg-panel-inset hover:text-[var(--color-accent)]"
          >
            {t("chatView.statusCard.detail")}
            <ExternalLink size={11} />
          </button>
        )}
      </div>

      {/* Detail block — global StatRow grammar. Status is in the header, so the
          rows carry run identity + live progress: 会话 / 当前步骤 / 模型 / 工作区. */}
      <div className="mx-2 mb-2 rounded-md bg-panel-inset px-3 py-2">
        <StatRow
          label={t("chatView.statusCard.labelTitle")}
          value={title?.trim() || t("chatView.statusCard.untitled")}
        />
        <StatRow label={t("chatView.statusCard.labelStep")} value={step} />
        <StatRow
          label={t("chatView.statusCard.labelModel")}
          value={model ? (provider ? `${model} · ${provider}` : model) : undefined}
        />
        <StatRow
          label={t("chatView.statusCard.labelWorkspace")}
          value={workspaceName ?? t("chatView.statusCard.workspaceAgentHome")}
        />
      </div>

      {/* Dynamic region — grows downward, one block per pending item. */}
      {hasActions && (
        <div className="flex flex-col gap-2 border-t border-border-divider px-3 py-2">
          {serverError && (
            <div className="flex items-start gap-2">
              <div className="min-w-0 flex-1">
                <div className="font-medium text-red-500">{t("chatView.statusCard.errorTitle")}</div>
                <div className="mt-0.5 break-words text-text-secondary">{serverError.content}</div>
              </div>
              <button
                type="button"
                onClick={dismissError}
                className="shrink-0 rounded-md border border-border-outer px-2 py-0.5 text-text-tertiary transition-colors hover:bg-panel-inset"
              >
                {t("chatView.statusCard.dismiss")}
              </button>
            </div>
          )}

          {needsContinue && (
            <div className="flex items-center gap-2">
              <div className="min-w-0 flex-1 text-amber-600 dark:text-amber-400">
                {pausedDetail?.message || t("chatView.statusCard.pausedIteration")}
              </div>
              <button
                type="button"
                onClick={() => void doContinue()}
                disabled={busyKey === "continue"}
                className="shrink-0 rounded-md px-2 py-0.5 font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
                style={{ backgroundColor: "var(--color-accent-strong)" }}
              >
                {t("chatView.statusCard.continue")}
              </button>
            </div>
          )}

          {approvals.map((a) => {
            const key = a.tool_call_id || a.request_id;
            const busy = busyKey === key;
            return (
              <div key={key} className="flex flex-col gap-1">
                <div className="min-w-0">
                  <span className="font-medium text-text-secondary">{a.tool_name}</span>
                  <span
                    className={cn(
                      "ml-1.5 rounded px-1 py-0.5 text-10 font-medium",
                      a.risk_level === "High" && "bg-red-100 text-red-700 dark:bg-red-900/40 dark:text-red-400",
                      a.risk_level === "Medium" && "bg-amber-100 text-amber-700 dark:bg-amber-900/40 dark:text-amber-400",
                      a.risk_level === "Low" && "bg-zinc-100 text-zinc-600 dark:bg-zinc-700/50 dark:text-zinc-300",
                    )}
                  >
                    {a.risk_level}
                  </span>
                  {a.params_summary && (
                    <div className="mt-0.5 truncate text-text-tertiary" title={a.params_summary}>
                      {a.params_summary}
                    </div>
                  )}
                </div>
                <div className="flex items-center gap-1">
                  <button
                    type="button"
                    onClick={() => void decideApproval("deny", a)}
                    disabled={busy}
                    className="rounded-md border border-zinc-300 px-2 py-0.5 font-medium text-text-secondary transition-colors hover:bg-zinc-200 disabled:opacity-50 dark:border-zinc-500 dark:hover:bg-zinc-600"
                  >
                    {t("chatView.statusCard.deny")}
                  </button>
                  <button
                    type="button"
                    onClick={() => void decideApproval("allow", a)}
                    disabled={busy}
                    className="rounded-md px-2 py-0.5 font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50"
                    style={{ backgroundColor: "var(--color-accent-strong)" }}
                  >
                    {t("chatView.statusCard.allow")}
                  </button>
                </div>
              </div>
            );
          })}
        </div>
      )}
    </div>
  );
}

/** label-left / monospace-value-right row — the right-panel StatRow, kept
 *  local so the card matches the global grammar without importing panel code. */
function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex items-center justify-between gap-2 py-1">
      <span className="shrink-0 text-text-tertiary">{label}</span>
      <span className="min-w-0 truncate font-mono text-text-secondary" title={value}>
        {value ?? "\u2014"}
      </span>
    </div>
  );
}
