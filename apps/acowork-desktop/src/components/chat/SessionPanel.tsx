import { useEffect, useLayoutEffect, useState, useRef } from "react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { MessageSquarePlus, Clock, MessageCircle, ChevronDown, Loader2, Trash2 } from "lucide-react";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { isProcessing } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { Tooltip } from "../common/Tooltip";
import { useReadOnlySessionIds } from "../../lib/session-write-access";

interface SessionPanelProps {
  agentId: string;
  onOpenMemory?: () => void;
}

/** Format a date string to relative time */
function formatRelativeTime(dateStr: string, t: (key: string, options?: Record<string, unknown>) => string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diffMs = now.getTime() - date.getTime();
  const diffSec = Math.floor(diffMs / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  if (diffSec < 60) return t("time.justNow");
  if (diffMin < 60) return t("time.minutesAgo", { count: diffMin });
  if (diffHour < 24) return t("time.hoursAgo", { count: diffHour });
  if (diffDay < 30) return t("time.daysAgo", { count: diffDay });
  return date.toLocaleDateString("zh-CN", { month: "short", day: "numeric" });
}

export function SessionPanel({ agentId }: SessionPanelProps) {
  const { t } = useTranslation();
  const agentStorage = useAgentStore((s) => s.agents[agentId]);
  const sessions = agentStorage?.sessions ?? [];
  const isLoading = agentStorage?.isLoading ?? false;
  const fetchSessions = useAgentStore((s) => s.fetchSessions);
  const createSession = useAgentStore((s) => s.createSession);
  const deleteSession = useAgentStore((s) => s.deleteSession);
  // ADR-076: a session shared with us is owner-only for delete.
  const readOnlyIds = useReadOnlySessionIds(agentId);

  const currentSessionId = useChatStore((s) => s.agentStates[agentId]?.activeSessionId ?? null);
  const [open, setOpen] = useState(false);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  const ref = useRef<HTMLDivElement>(null);
  const listRef = useRef<HTMLDivElement>(null);

  // Fetch sessions when panel opens or agent changes
  useEffect(() => {
    if (agentId && open) {
      void fetchSessions(agentId);
    }
  }, [agentId, fetchSessions, open]);

  // Close on outside click
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // Force scrollbar recalculation when sessions change (fixes stale scrollbar after agent switch)
  // useLayoutEffect runs synchronously after DOM mutations, before paint
  useLayoutEffect(() => {
    if (listRef.current) {
      const el = listRef.current;
      // Force browser to recalculate overflow geometry
      el.style.overflowY = 'hidden';
      void el.offsetHeight;
      el.style.overflowY = '';
    }
  }, [sessions]);

  // ADR-038: opening a session from the history list is a "first-open"
  // scenario (or a re-open after close), so we delegate to
  // `chatStore.openSession` which combines the UI tab + MQTT open_session
  // + HTTP message reload into one atomic transition.
  const handleSwitchSession = async (sessionId: string) => {
    await useChatStore.getState().openSession(agentId, sessionId);
    setOpen(false);
  };

  const handleDeleteSession = async (sessionId: string) => {
    if (deletingId) return;
    if (readOnlyIds.has(sessionId)) return;
    setDeletingId(sessionId);
    try {
      await deleteSession(agentId, sessionId);
    } finally {
      setDeletingId(null);
      setConfirmDelete(null);
    }
  };

  return (
    <div ref={ref} className="relative">
      {/* Trigger button */}
      <button
        onClick={() => setOpen(!open)}
        className="inline-flex items-center gap-1.5 rounded-md px-2 py-1.5 text-xs transition-colors text-text-tertiary hover:bg-[var(--color-accent)]/10 dark:hover:bg-[var(--color-accent)]/15 hover:text-text-secondary"
      >
        <MessageCircle size={14} />
        Session
        <ChevronDown className={cn("h-3 w-3 transition-transform", open && "rotate-180")} />
      </button>

      {/* Dropdown menu */}
      {open && (
        <div className="absolute bottom-full left-0 mb-2 w-72 rounded-md border border-border-outer bg-modal-surface shadow-lg z-50">
          {/* Session list */}
          <div ref={listRef} className="max-h-80 overflow-y-auto py-1">
            {isLoading && sessions.length === 0 && (
              <div className="flex items-center justify-center gap-2 px-3 py-6 text-xs text-text-tertiary ">
                <Loader2 className="h-3.5 w-3.5 animate-spin" />
                {t("sessionPanel.loadingSessions")}
              </div>
            )}

            {!isLoading && sessions.length === 0 && (
              <div className="px-3 py-4 text-center text-xs text-text-tertiary ">
                {t("sessionPanel.noSessions")}
              </div>
            )}

            {sessions.map((session) => {
              const isActive = session.session_id === currentSessionId;
              const isDeleting = confirmDelete === session.session_id;
              // ADR-021: Derive streaming status purely from sessionStatus (backend source of truth)
              const isStreaming = (() => {
                const agent = useChatStore.getState().agentStates[agentId];
                const sessionState = agent?.sessionStates[session.session_id];
                if (!sessionState) return false;
                return isProcessing(sessionState.sessionStatus);
              })();
              return (
                <div
                  key={session.session_id}
                  className={`group flex items-center gap-2 px-3 py-2 transition-colors ${listRowHover}`}
                >
                  {/* Select session button */}
                  <button
                    onClick={() => handleSwitchSession(session.session_id)}
                    className="flex min-w-0 flex-1 flex-col gap-0.5 text-left"
                  >
                    <div className="flex items-center gap-2">
                      {isStreaming ? (
                        <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin text-[var(--color-accent)]" />
                      ) : (
                        <MessageCircle className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
                      )}
                      <span className={cn("min-w-0 flex-1 truncate text-xs", isActive && "font-semibold text-[var(--color-accent)]", !isActive && "text-text-secondary ")}>
                        {session.title || t("sessionPanel.untitledSession")}
                      </span>
                    </div>
                    <div className="ml-5.5 flex items-center gap-2 text-10 text-text-tertiary ">
                      <span className="flex items-center gap-1">
                        <Clock className="h-3 w-3" />
                        {formatRelativeTime(session.created_at, t)}
                      </span>
                      <span>·</span>
                      <span>{t("sessionPanel.messages", { count: session.message_count })}</span>
                    </div>
                  </button>

                  {/* Delete button */}
                  {isDeleting ? (
                    <div className="flex items-center gap-1">
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          void handleDeleteSession(session.session_id);
                        }}
                        disabled={deletingId !== null}
                        className="rounded btn-accent px-2 py-0.5 text-xs disabled:opacity-50 disabled:cursor-not-allowed"
                      >
                        {t("common.delete")}
                      </button>
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          setConfirmDelete(null);
                        }}
                        className="rounded btn-solid px-2 py-0.5 text-xs"
                      >
                        {t("common.cancel")}
                      </button>
                    </div>
                  ) : (
                    <Tooltip
                      content={readOnlyIds.has(session.session_id) ? t("chatPanel.readOnlySession") : t("sessionPanel.deleteSession")}
                      variant="plain"
                    >
                      <button
                        onClick={(e) => {
                          e.stopPropagation();
                          setConfirmDelete(session.session_id);
                        }}
                        disabled={deletingId !== null || readOnlyIds.has(session.session_id)}
                        className="rounded p-1 text-text-tertiary opacity-0 transition-all group-hover:opacity-100 hover:bg-danger-surface hover:text-danger-text disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent"
                      >
                        <Trash2 className="h-3 w-3" />
                      </button>
                    </Tooltip>
                  )}
                </div>
              );
            })}
          </div>

          {/* Footer: New conversation */}
          <div className="border-t border-border-divider p-2">
            <button
              onClick={() => {
                createSession(agentId);
                setOpen(false);
              }}
              className="mx-1.5 flex w-[calc(100%-0.75rem)] items-center justify-center gap-1.5 rounded-md bg-surface-hover px-3 py-2 text-xs font-medium text-text-secondary transition-colors hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
            >
              <MessageSquarePlus className="h-3.5 w-3.5" />
              {t("sessionPanel.newConversation")}
            </button>
          </div>
        </div>
      )}
    </div>
  );
}
