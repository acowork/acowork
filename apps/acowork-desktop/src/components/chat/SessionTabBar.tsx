import { useState, useRef, useEffect, useMemo } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { isProcessing } from "../../lib/types";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import {
  ContextMenu,
  useContextMenu,
  type ContextMenuItem,
} from "../common/ContextMenu";
import { Plus, Clock, X, Trash2, ChevronLeft, ChevronRight, Search, TriangleAlert, XSquare, Pencil } from "lucide-react";
import { OutlineChatIcon, FilledChatIcon } from "../common/ChatIcon";
import { OutlineBrainIcon, FilledBrainIcon } from "../common/BrainIcon";
import { OutlineChatHistoryIcon } from "../common/ChatHistoryIcon";
import { StyledInput } from "../common/StyledInput";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ScrollableTabBar, type ScrollableTabBarHandle } from "../common/ScrollableTabBar";
import { TabItem } from "../common/tab";
import { Tooltip } from "../common/Tooltip";
import { useReadOnlySessionIds } from "../../lib/session-write-access";
import { useChatSessionStore } from "../../stores/chatSessionStore";

const EMPTY_ARRAY: string[] = [];

// ── Relative time formatter ──────────────────────────────────────────────

function formatRelativeTime(dateStr: string, t: (key: string, options?: Record<string, unknown>) => string): string {
  const date = new Date(dateStr);
  const now = new Date();
  const diffSec = Math.floor((now.getTime() - date.getTime()) / 1000);
  const diffMin = Math.floor(diffSec / 60);
  const diffHour = Math.floor(diffMin / 60);
  const diffDay = Math.floor(diffHour / 24);

  if (diffSec < 60) return t("time.justNow");
  if (diffMin < 60) return t("time.minutesAgo", { count: diffMin });
  if (diffHour < 24) return t("time.hoursAgo", { count: diffHour });
  if (diffDay < 30) return t("time.daysAgo", { count: diffDay });
  return date.toLocaleDateString("en", { month: "short", day: "numeric" });
}

// ── SessionListDropdown ──────────────────────────────────────────────────

interface SessionListDropdownProps {
  agentId: string;
  /** Session currently shown in the chat area. Used to mirror the title
   *  bar's "selected = accent" rule inside the dropdown so both surfaces
   *  speak the same visual language (see icon block below). */
  activeSessionId: string | undefined;
  onClose: () => void;
  /** Called after a session was selected and opened. When the Agent page
   *  has the pinned chat tab (WeChat model, design 27 §5.4.4), activating
   *  an execution session must flip the surface to exec — the tab bar
   *  itself does not care (it already lives there). */
  onOpened?: () => void;
}

export function SessionListDropdown({ agentId, activeSessionId, onClose, onOpened }: SessionListDropdownProps) {
  const { t } = useTranslation();
  const agentStorage = useAgentStore((s) => s.agents[agentId]);
  const sessions = agentStorage?.sessions ?? [];
  const totalCount = agentStorage?.pagination.totalCount ?? 0;
  const currentPage = agentStorage?.pagination.currentPage ?? 1;
  const totalPages = agentStorage?.pagination.totalPages ?? 1;
  const pageSize = agentStorage?.pagination.pageSize ?? 20;
  const fetchSessions = useAgentStore((s) => s.fetchSessions);
  const deleteSession = useAgentStore((s) => s.deleteSession);
  const clearAllSessions = useAgentStore((s) => s.clearAllSessions);
  const openSessionIds = useChatStore((s) => s.agentStates[agentId]?.openSessionIds ?? EMPTY_ARRAY);
  // ADR-076: sessions shared *with* us are read-only — delete is owner-only
  // and would 404. The row still opens for reading.
  const readOnlyIds = useReadOnlySessionIds(agentId);
  const [confirmDelete, setConfirmDelete] = useState<string | null>(null);
  const [deletingId, setDeletingId] = useState<string | null>(null);
  // Clear-all: the dropdown header's trash button opens a confirmation
  // modal (destructive bulk action — same pattern as the looping-session
  // close dialog in SessionTabBar).
  const [confirmClearAll, setConfirmClearAll] = useState(false);
  const [clearing, setClearing] = useState(false);
  const [searchTerm, setSearchTerm] = useState("");
  const ref = useRef<HTMLDivElement>(null);

  useEffect(() => {
    void fetchSessions(agentId, 1);
  }, [agentId, fetchSessions]);

  // Close on outside click
  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [onClose]);

  // ADR-038: selecting a session from the history dropdown is a "first-open"
  // event from the user's POV — it may or may not be in `openSessionIds`.
  // We delegate to `chatStore.openSession`, which combines the UI half
  // (open tab) with the backend half (MQTT open_session + HTTP reload).
  const handleSelect = async (sessionId: string) => {
    await useChatStore.getState().openSession(agentId, sessionId);
    onOpened?.();
    onClose();
  };

  const handleDelete = async (sessionId: string) => {
    if (deletingId) return;
    if (readOnlyIds.has(sessionId)) return;
    setDeletingId(sessionId);
    try {
      await deleteSession(agentId, sessionId);
      // Also close the tab if open
      if (openSessionIds.includes(sessionId)) {
        useChatStore.getState().closeTab(agentId, sessionId);
      }
      // Design 27 (v2.1): the pinned chat tab is the landing surface — when
      // the delete took the last open exec tab with it, fall back to chat
      // instead of a blank exec view (mirrors `finishCloseTab`). `agentStore
      // .deleteSession` deliberately no longer re-opens a background session.
      if (useChatStore.getState().getOpenSessionIds(agentId).length === 0) {
        const st = useChatSessionStore.getState().byAgent[agentId];
        if (st?.chatSessionId) useChatSessionStore.getState().setMode(agentId, "chat");
      }
    } finally {
      setDeletingId(null);
      setConfirmDelete(null);
    }
  };

  const handlePageChange = (page: number) => {
    void fetchSessions(agentId, page);
  };

  const handleClearAll = async () => {
    if (clearing) return;
    setClearing(true);
    try {
      await clearAllSessions(agentId);
      setConfirmClearAll(false);
      // Design 27 (v2.1): clear-all emptied the exec strip — the pinned chat
      // tab is the landing surface, not a blank exec view. No auto-reopen
      // (same rule as `finishCloseTab` / `handleDelete`).
      const st = useChatSessionStore.getState().byAgent[agentId];
      if (st?.chatSessionId) useChatSessionStore.getState().setMode(agentId, "chat");
    } catch {
      // agentStore already logs the failure; keep the modal open so the
      // user sees it did not go through.
    } finally {
      setClearing(false);
    }
  };

  const start = (currentPage - 1) * pageSize + 1;
  const end = Math.min(currentPage * pageSize, totalCount);

  // Client-side search filter
  const filteredSessions = searchTerm.trim()
    ? sessions.filter((s) => s.title?.toLowerCase().includes(searchTerm.toLowerCase()))
    : sessions;

  return (
    <div
      ref={ref}
      className="absolute right-0 top-full mt-1 w-72 rounded-md border border-border-outer bg-modal-surface shadow-lg z-50"
    >
      {/* Header with total count */}
      <div className="flex items-center justify-between border-b border-border-divider px-3 py-1.5 text-11 text-text-tertiary">
        <span>
          {totalCount > 0 ? (
            <>{t("sessionTabBar.sidebarShowing", { start, end, total: totalCount })}</>
          ) : (
            <>{t("sessionTabBar.sidebarNoSessions")}</>
          )}
        </span>
        {totalCount > 0 && (
          <Tooltip content={t("sessionTabBar.clearAll")} variant="plain">
            <button
              onClick={() => setConfirmClearAll(true)}
              className="rounded p-1 transition-colors hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
            >
              <Trash2 className="h-3 w-3" />
            </button>
          </Tooltip>
        )}
      </div>

      {/* Search input */}
      <div className="border-b border-border-divider px-2 py-1.5">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 -translate-y-1/2 h-3 w-3 text-text-tertiary" />
          <StyledInput
            type="text"
            value={searchTerm}
            onChange={(e) => setSearchTerm(e.target.value)}
            placeholder={t("sessionTabBar.sidebarSearchPlaceholder")}
            className="pl-7"
          />
        </div>
      </div>

      <OverlayScrollbar maxHeight={320} className="py-1">
        {filteredSessions.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-text-tertiary ">
            {t("sessionTabBar.sidebarNoSessionsYet")}
          </div>
        )}

        {filteredSessions.map((session) => {
          const isOpen = openSessionIds.includes(session.session_id);
          const isDeleting = confirmDelete === session.session_id;
          const rowReadOnly = readOnlyIds.has(session.session_id);
          const sessionState = useChatStore.getState().getSessionState(agentId, session.session_id);
          const isProc = isProcessing(sessionState?.sessionStatus);
          // Mirror the SessionTabBar's "selected = accent colour" rule so the
          // dropdown and the title bar speak one visual language: the entry
          // for the session currently shown in the chat area is the only one
          // rendered in --color-accent, regardless of how many sessions are
          // "open" (open tabs). All other entries stay muted zinc.
          const isSelected = session.session_id === activeSessionId;

          return (
            <div
              key={session.session_id}
              className={`group flex items-center gap-2 px-3 py-2 transition-colors ${listRowHover}`}
            >
              <button
                onClick={() => handleSelect(session.session_id)}
                className="flex min-w-0 flex-1 flex-col gap-0.5 text-left"
              >
                <div className="flex items-center gap-2">
                  {/* Icon — same four-state matrix as the SessionTabBar
                      title tabs so the dropdown matches the title bar,
                      but drawn with the BRAIN pair: execution sessions
                      read as "the agent thinking", the chat bubble stays
                      reserved for the Chat surface (design 27 §6.1):
                        • selected + idle        → FilledBrainIcon accent
                        • selected + processing  → FilledBrainIcon accent + animate-pulse
                        • unselected + idle      → OutlineBrainIcon muted zinc
                        • unselected + processing → FilledBrainIcon foreground + animate-pulse
                      The old <Loader2 animate-spin> is gone — processing is
                      now expressed via the same breathing animation used in
                      the title bar, keeping one global "still working" cue
                      across the whole chat area. */}
                  {isSelected ? (
                    <FilledBrainIcon
                      className={cn(
                        "h-3.5 w-3.5 shrink-0 text-[var(--color-accent)]",
                        isProc && "animate-pulse",
                      )}
                    />
                  ) : isProc ? (
                    <FilledBrainIcon className="h-3.5 w-3.5 shrink-0 animate-pulse text-text-secondary " />
                  ) : (
                    <OutlineBrainIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
                  )}
                  <span className={cn("min-w-0 flex-1 truncate text-xs text-text-secondary ")}>
                    {session.title || "Untitled session"}
                  </span>
                  {isOpen && (
                    <span className="h-1.5 w-1.5 shrink-0 rounded-full bg-[var(--color-accent)]" />
                  )}
                </div>
                <div className="ml-5.5 flex items-center gap-2 text-10 text-text-tertiary ">
                  <span>{formatRelativeTime(session.created_at, t)}</span>
                  <span>·</span>
                  <span>{session.message_count} msg</span>
                </div>
              </button>

              {isDeleting ? (
                <div className="flex items-center gap-1">
                  <button
                    onClick={(e) => { e.stopPropagation(); void handleDelete(session.session_id); }}
                    disabled={deletingId !== null}
                    className="rounded btn-accent px-2 py-0.5 text-xs disabled:opacity-50"
                  >
                    {t("common.delete")}
                  </button>
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(null); }}
                    className="rounded btn-solid px-2 py-0.5 text-xs"
                  >
                    {t("common.cancel")}
                  </button>
                </div>
              ) : (
                <Tooltip
                  content={rowReadOnly ? t("chatPanel.readOnlySession") : t("sessionTabBar.deleteSession")}
                  variant="plain"
                >
                  <button
                    onClick={(e) => { e.stopPropagation(); setConfirmDelete(session.session_id); }}
                    disabled={deletingId !== null || rowReadOnly}
                    className="rounded p-1 text-text-tertiary opacity-0 transition-all group-hover:opacity-100 hover:bg-red-50 hover:text-red-600 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent dark:hover:bg-red-900/20 dark:hover:text-red-400"
                  >
                    <Trash2 className="h-3 w-3" />
                  </button>
                </Tooltip>
              )}
            </div>
          );
        })}
      </OverlayScrollbar>

      {/* Pagination */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-border-divider px-1 py-1.5">
          <button
            onClick={() => handlePageChange(currentPage - 1)}
            disabled={currentPage <= 1}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30  dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-11 text-text-tertiary ">
            Page {currentPage} of {totalPages}
          </span>
          <button
            onClick={() => handlePageChange(currentPage + 1)}
            disabled={currentPage >= totalPages}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30  dark:hover:bg-zinc-800"
          >
            <ChevronRight className="h-3.5 w-3.5" />
          </button>
        </div>
      )}

      {/* Clear-all confirmation dialog. Rendered inside the dropdown's
          ref div so clicks in the modal do not trip the outside-click
          close handler. */}
      {confirmClearAll && (
        <div
          className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay"
          onClick={() => !clearing && setConfirmClearAll(false)}
        >
          <div
            className="mx-4 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface p-5 shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-start gap-3">
              <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-red-100 dark:bg-red-900/30">
                <TriangleAlert className="h-5 w-5 text-red-600 dark:text-red-400" />
              </div>
              <div className="flex-1">
                <h3 className="text-sm font-medium text-text ">
                  {t("sessionTabBar.clearAllConfirmTitle")}
                </h3>
                <p className="mt-1 text-xs text-text-tertiary">
                  {t("sessionTabBar.clearAllConfirmWarning", { count: totalCount })}
                </p>
              </div>
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setConfirmClearAll(false)}
                disabled={clearing}
                className="rounded btn-solid px-3 py-1.5 text-xs disabled:opacity-50"
              >
                {t("sessionTabBar.cancel")}
              </button>
              <button
                onClick={() => void handleClearAll()}
                disabled={clearing}
                className="rounded bg-red-600 px-3 py-1.5 text-xs text-white transition-colors hover:bg-red-700 disabled:opacity-50"
              >
                {clearing ? t("sessionTabBar.clearing") : t("sessionTabBar.clearAllConfirm")}
              </button>
            </div>
          </div>
        </div>
      )}
    </div>
  );
}

// ── SessionTabBar ────────────────────────────────────────────────────────

interface SessionTabBarProps {
  agentId: string;
  /** Pinned Chat tab (WeChat model, design 27 §5.4.4): the pair's single
   *  Chat session, first in the strip, labelled with the agent's name,
   *  never closable/renamable. Execution sessions are the remaining tabs —
   *  opening or closing them never removes this one. When omitted (local
   *  mode / chat surface unreachable) the bar behaves as before. */
  chatTab?: {
    label: string;
    active: boolean;
    onSelect: () => void;
  };
  /** Opens the in-conversation history dialog (ADR-097) for the pair's Chat
   *  session. Rendered as a tile just LEFT of the `+` button, shown only while
   *  the Chat tab is the active surface — the entry targets the durable Chat
   *  transcript, so it is meaningless over an execution session. When omitted
   *  (local mode / chat surface unreachable) the bar renders no history tile. */
  onOpenChatHistory?: () => void;
}

export function SessionTabBar({ agentId, chatTab, onOpenChatHistory }: SessionTabBarProps) {
  const { t } = useTranslation();
  const agent = useChatStore((s) => s.agentStates[agentId]);
  const openSessionIds = agent?.openSessionIds ?? [];
  const activeSessionId = agent?.activeSessionId;
  const sessions = useAgentStore((s) => s.agents[agentId]?.sessions ?? EMPTY_ARRAY);
  const { createSession, closeSession, renameSession } = useAgentStore();
  // ADR-076: renaming a session shared with us is owner-only; the tab
  // itself still opens for reading.
  const readOnlyIds = useReadOnlySessionIds(agentId);
  const setActiveTab = useChatStore((s) => s.setActiveTab);
  const openSession = useChatStore((s) => s.openSession);

  const [listOpen, setListOpen] = useState(false);
  const [closingSessionId, setClosingSessionId] = useState<string | null>(null);
  // Right-click context menu on session tabs. Payload is the sessionId of
  // the tab the user right-clicked. Outside-click / Escape / portal
  // positioning are all owned by `useContextMenu`.
  const tabMenu = useContextMenu<{ sessionId: string }>();
  const scrollableRef = useRef<ScrollableTabBarHandle>(null);

  // Inline rename state. `renamingSessionId` is the session whose tab title
  // is currently rendered as an input; `renameValue` is the keystroke buffer.
  const [renamingSessionId, setRenamingSessionId] = useState<string | null>(null);
  const [renameValue, setRenameValue] = useState("");
  const renameInputRef = useRef<HTMLInputElement>(null);
  const lastRenamingIdRef = useRef<string | null>(null);
  // Mirrors `renamingSessionId` for event handlers that may fire during
  // unmount (e.g. a stale `blur` after Escape), where the React state closure
  // can still hold the pre-cancel value.
  const renamingSessionIdRef = useRef<string | null>(null);

  // Seed + focus + select the rename input only on the false→true transition,
  // so re-renders (e.g. optimistic title patch) never wipe the user's typing.
  useEffect(() => {
    if (renamingSessionId && lastRenamingIdRef.current !== renamingSessionId) {
      // Read the latest session title imperatively so this effect does not
      // re-run (and cancel the focus animation frame) on unrelated session
      // list updates while the rename input is open. Live overlay first —
      // that is what the tab itself displays.
      const live = useChatStore
        .getState()
        .getSessionState(agentId, renamingSessionId)?.title;
      const session = useAgentStore
        .getState()
        .agents[agentId]?.sessions.find((s) => s.session_id === renamingSessionId);
      const seed = live || session?.title || t("sessionTabBar.untitled");
      setRenameValue(seed);
      const frame = requestAnimationFrame(() => {
        const input = renameInputRef.current;
        if (input) {
          input.focus();
          input.select();
        }
      });
      lastRenamingIdRef.current = renamingSessionId;
      return () => cancelAnimationFrame(frame);
    }
    if (!renamingSessionId) lastRenamingIdRef.current = null;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [renamingSessionId, agentId, t]);

  // Get title for a session — live per-session overlay first (fed by the
  // config mapper, like model/provider), then the `fetchSessions` snapshot.
  const getTitle = (sessionId: string): string => {
    const live = agent?.sessionStates[sessionId]?.title;
    const snapshot = sessions.find((s) => s.session_id === sessionId)?.title;
    return live || snapshot || t("sessionTabBar.untitled");
  };

  // Get status for a session tab
  const getStatus = (sessionId: string) => {
    const state = useChatStore.getState().getSessionState(agentId, sessionId);
    return state?.sessionStatus;
  };

  // ADR-038: clicking on a tab in the strip is "切换前台" (switching
  // foreground) — the session is *already* in `openSessionIds` and has been
  // confirmed Active on the backend. We use the strict no-side-effect
  // `setActiveTab`. With a pinned chat tab present, any exec-tab click also
  // flips the surface back to exec (co-existence model — the chat surface
  // stays one click away in the same strip).
  const showExecSurface = () => {
    const st = useChatSessionStore.getState().byAgent[agentId];
    if (st?.chatSessionId) useChatSessionStore.getState().setMode(agentId, "exec");
  };

  const handleTabClick = (sessionId: string) => {
    // Ignore clicks that ended a drag
    if (scrollableRef.current?.hasMoved.current) return;
    showExecSurface();
    if (sessionId === activeSessionId) return;
    setActiveTab(agentId, sessionId);
  };

  const handleClose = async (e: React.MouseEvent, sessionId: string) => {
    e.stopPropagation();
    const status = getStatus(sessionId);

    // If session is looping, ask confirmation before closing
    if (isProcessing(status)) {
      setClosingSessionId(sessionId);
      return;
    }

    await closeSession(agentId, sessionId);
    finishCloseTab(sessionId);
  };

  const confirmClose = async () => {
    if (!closingSessionId) return;
    const sid = closingSessionId;
    setClosingSessionId(null);
    await closeSession(agentId, sid);
    finishCloseTab(sid);
  };

  const finishCloseTab = (sessionId: string) => {
    void useChatStore.getState().closeTab(agentId, sessionId).then((newActiveId) => {
      // If the closed tab was active, switch to the new active
      if (sessionId === activeSessionId && newActiveId) {
        // ADR-038: the new active session is now in openSessionIds (the
        // neighbor was the immediate right-or-left sibling). It is the
        // "still open" case, so `setActiveTab` is the right verb.
        useChatStore.getState().setActiveTab(agentId, newActiveId);
      }

      // If no tabs remain: with a pinned chat tab the strip still has a
      // landing surface, so fall back to the chat view instead of
      // auto-reopening a session. Without the chat tab (legacy fallback)
      // the old rule holds — reopening an existing session (not creating
      // one, which would loop) keeps the chat area non-blank.
      const remaining = useChatStore.getState().getOpenSessionIds(agentId);
      if (remaining.length === 0) {
        if (chatTab) {
          useChatSessionStore.getState().setMode(agentId, "chat");
          return;
        }
        const sessions = useAgentStore.getState().agents[agentId]?.sessions;
        const otherSession = sessions?.find((s: { session_id: string }) => s.session_id !== sessionId);
        if (otherSession) {
          // ADR-038: this session is not yet in openSessionIds (it was a
          // background session from the sidebar), so we need the full
          // openSession path (UI + backend).
          void openSession(agentId, otherSession.session_id);
        } else {
          // No Chat tab on this agent (legacy exec-only surface), so the
          // creation has no conversation to belong to — deliberately
          // unbound (ADR-099).
          createSession(agentId);
        }
      }
    });
  };

  // `+` is the deterministic "new task" entry: when this agent has a Chat
  // session the new execution session is bound to it, so the user works in
  // the exec view (which never consults the Chat Router) and the session
  // still shows up as a candidate when they later refer to that task from
  // the chat. Binding is mode-agnostic on purpose — the pinned chat tab is
  // the agent's conversation whether or not it is on screen.
  const handleNew = () => {
    createSession(agentId, { bindToChat: true });
    showExecSurface();
  };

  // ── Inline rename ──────────────────────────────────────────────────────
  const beginRename = (sessionId: string) => {
    if (readOnlyIds.has(sessionId)) return;
    renamingSessionIdRef.current = sessionId;
    setRenamingSessionId(sessionId);
  };

  const commitRename = () => {
    const sessionId = renamingSessionIdRef.current;
    if (!sessionId) return;
    renamingSessionIdRef.current = null;
    setRenamingSessionId(null);
    const trimmed = renameValue.trim();
    // Compare against the *displayed* title (live overlay first, same order
    // as `getTitle`), so an untitled tab seeded with the localized
    // "Untitled" fallback is treated as unchanged on Enter.
    const current =
      agent?.sessionStates[sessionId]?.title ||
      sessions.find((s) => s.session_id === sessionId)?.title ||
      t("sessionTabBar.untitled");
    // Empty / unchanged → close without a server roundtrip.
    if (!trimmed || trimmed === current) return;
    void renameSession(agentId, sessionId, trimmed);
  };

  const cancelRename = () => {
    renamingSessionIdRef.current = null;
    setRenamingSessionId(null);
  };

  const handleRenameKeyDown = (e: React.KeyboardEvent<HTMLInputElement>) => {
    if (e.key === "Enter") {
      e.preventDefault();
      commitRename();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancelRename();
    }
  };

  // ── Session tab right-click menu ──────────────────────────────────────
  const handleTabContextMenu = (e: React.MouseEvent, sessionId: string) => {
    tabMenu.openAt(e, { sessionId });
  };

  // Close the right-clicked session itself. If it's currently looping,
  // defer to the existing confirm dialog; otherwise close immediately.
  const handleContextCloseChat = async (sessionId: string) => {
    const status = getStatus(sessionId);
    if (isProcessing(status)) {
      setClosingSessionId(sessionId);
      return;
    }
    await closeSession(agentId, sessionId);
    finishCloseTab(sessionId);
  };

  // Close every other open session, keeping the right-clicked one.
  // Mirrors VS Code's "Close Others" — runs sequentially to give each
  // session a chance to switch the active selection cleanly.
  const handleContextCloseOthers = async (keepSessionId: string) => {
    const others = openSessionIds.filter((id) => id !== keepSessionId);
    for (const id of others) {
      // Skip sessions currently streaming — closing them would interrupt
      // a live response. The user can right-click them individually to
      // get the explicit confirm dialog.
      const status = getStatus(id);
      if (isProcessing(status)) continue;
      await closeSession(agentId, id);
    }
  };

  const handleContextOpenNewSession = () => {
    createSession(agentId, { bindToChat: true });
    showExecSurface();
  };

  // Memoised menu items. Built only when the right-clicked session or
  // the open-tab count changes (which gates the disabled flags).
  const tabMenuItems = useMemo<ContextMenuItem<{ sessionId: string }>[]>(() => {
    const sid = tabMenu.payload?.sessionId;
    if (!sid) return [];
    // With the pinned chat tab, closing every exec session is safe — the
    // chat surface is the guaranteed landing spot. Legacy mode still keeps
    // one tab open.
    const canClose = chatTab ? true : openSessionIds.length > 1;
    const sidReadOnly = readOnlyIds.has(sid);
    const items: ContextMenuItem<{ sessionId: string }>[] = [
      {
        key: "rename",
        icon: <Pencil size={14} />,
        label: t("sessionTabBar.rename"),
        // ADR-076: renaming a session shared with us is owner-only.
        disabled: sidReadOnly,
        onClick: ({ payload }) => payload && beginRename(payload.sessionId),
      },
      {
        key: "close-chat",
        icon: <X size={14} />,
        label: t("sessionTabBar.closeChat"),
        disabled: !canClose,
        dividerBefore: true,
        // Disable when only one tab remains — keep at least one open so
        // the chat area never ends up empty.
        onClick: ({ payload }) => payload && handleContextCloseChat(payload.sessionId),
      },
      {
        key: "close-others",
        icon: <XSquare size={14} />,
        label: t("sessionTabBar.closeOtherChats"),
        disabled: !canClose,
        // Same guard: "Close others" is a no-op when there are no others.
        onClick: ({ payload }) => payload && handleContextCloseOthers(payload.sessionId),
      },
      {
        key: "new-session",
        icon: <Plus size={14} />,
        label: t("sessionTabBar.openNewSession"),
        dividerBefore: true,
        onClick: handleContextOpenNewSession,
      },
    ];
    return items;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [tabMenu.payload?.sessionId, openSessionIds.length, readOnlyIds, t]);

  // With a pinned chat tab the strip is never empty even before the
  // agent state exists (no exec sessions while the agent is stopped) —
  // the chat surface must stay reachable, so only bail without it.
  if (!agent && !chatTab) return null;

  // Single-focus rule (design 27 §6.1): exactly ONE tab reads as active
  // — whichever surface is in front. `chatStore.activeSessionId` keeps
  // pointing at the foreground execution session even while the chat
  // surface is shown (it is what ChatPanel restores when the user flips
  // back), so with the pinned chat tab active no exec tab may claim the
  // active style. Without a chat tab, the store's active session is the
  // foreground tab directly.
  const foregroundExecId = chatTab?.active ? undefined : activeSessionId;

  return (
    <div className="flex select-none px-0.5 gap-0.5 pt-[5px] border-b border-border-divider">
      <ScrollableTabBar
        ref={scrollableRef}
        activeItemSelector={
          chatTab?.active
            ? '[data-chat-tab="pinned"]'
            : foregroundExecId
              ? `[data-session-id="${foregroundExecId}"]`
              : undefined
        }
        activeItemId={chatTab?.active ? undefined : (foregroundExecId ?? undefined)}
      >
        {/* Pinned Chat tab — the pair's single conversation (WeChat model:
            label = agent name, no close button, no rename, no context
            menu). Two icon states only: filled accent when active, outline
            when not. It deliberately does NOT breathe on an in-flight
            turn — a chat turn always runs in an exec session, and that
            tab's brain icon already breathes; two pulsing icons side by
            side read as noise, not as status. */}
        {chatTab && (
          <TabItem
            data-chat-tab="pinned"
            key="__chat_pinned__"
            onClick={chatTab.onSelect}
            active={chatTab.active}
          >
            {chatTab.active ? (
              <FilledChatIcon className="h-3.5 w-3.5 shrink-0 text-[var(--color-accent)]" />
            ) : (
              <OutlineChatIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
            )}
            <span
              className={cn(
                "min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]",
                chatTab.active && "text-text-secondary ",
              )}
            >
              {chatTab.label}
            </span>
          </TabItem>
        )}
        {openSessionIds.map((sessionId) => {
          const isActive = sessionId === foregroundExecId;
          const status = getStatus(sessionId);
          const isProc = isProcessing(status);
          const rowReadOnly = readOnlyIds.has(sessionId);

          return (
            <TabItem
              data-session-id={sessionId}
              key={sessionId}
              onClick={() => handleTabClick(sessionId)}
              onContextMenu={(e) => handleTabContextMenu(e, sessionId)}
              active={isActive}
            >
              {/* Streaming indicator dot was removed — it crowded the icon.
                  The brain icon itself now breathes (animate-pulse) when an
                  unselected tab is still streaming, see below.
                  Exec icon — the brain pair (the chat bubble belongs to the
                  Chat surface only; design 27 §6.1).
                  Four visual states (selected vs unselected only differ by
                  colour, the breathing animation stays identical so they
                  read as one consistent "processing" cue):
                    • selected + idle        → FilledBrainIcon in
                      --color-accent (static, the global highlight colour,
                      most prominent).
                    • selected + processing  → FilledBrainIcon in
                      --color-accent with animate-pulse — same breathing
                      rhythm as the unselected variant, just in accent
                      colour so the user can still spot the active tab
                      while it's working.
                    • unselected + processing → FilledBrainIcon (solid
                      silhouette) in the normal foreground colour with
                      animate-pulse — solid + breathing distinguishes it
                      from both idle tabs and the accent-coloured selected
                      tab. Replaces the old pulsing dot.
                    • unselected + idle      → OutlineBrainIcon in muted zinc.
                  Hidden during inline rename so the input gets the full
                  slot. */}
              {renamingSessionId !== sessionId &&
                (isActive ? (
                  <FilledBrainIcon
                    className={cn(
                      "h-3.5 w-3.5 shrink-0 text-[var(--color-accent)]",
                      isProc && "animate-pulse",
                    )}
                  />
                ) : isProc ? (
                  <FilledBrainIcon className="h-3.5 w-3.5 shrink-0 animate-pulse text-text-secondary " />
                ) : (
                  <OutlineBrainIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
                ))}
              {/* Title — double-click enters inline rename (same action as the
                  right-click "Rename" menu item). */}
              {renamingSessionId === sessionId ? (
                <input
                  ref={renameInputRef}
                  type="text"
                  value={renameValue}
                  onChange={(e) => setRenameValue(e.target.value)}
                  onKeyDown={handleRenameKeyDown}
                  onBlur={commitRename}
                  onClick={(e) => e.stopPropagation()}
                  onMouseDown={(e) => e.stopPropagation()}
                  onDoubleClick={(e) => e.stopPropagation()}
                  onContextMenu={(e) => e.stopPropagation()}
                  aria-label={t("sessionTabBar.rename")}
                  className="min-w-0 flex-1 rounded-sm border border-[var(--color-accent)] bg-modal-surface px-1 text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)] text-text outline-none dark:bg-zinc-900 "
                />
              ) : (
                <span
                  onDoubleClick={(e) => {
                    e.stopPropagation();
                    beginRename(sessionId);
                  }}
                  // A read-only row stays a read-only *label*: no text cursor
                  // and an explanatory title, because `beginRename` silently
                  // ignores the double-click (a click that does nothing is
                  // worse than one that looks unclickable).
                  title={rowReadOnly ? t("chatPanel.readOnlySession") : undefined}
                  className={cn(
                    "min-w-0 flex-1 truncate text-[length:var(--tab-font-size)] leading-[var(--tab-line-height)]",
                    !rowReadOnly && "cursor-text",
                    isProc && isActive && "text-text-secondary ",
                  )}
                >
                  {getTitle(sessionId)}
                </span>
              )}
              {/* Close button — hidden when this is the only remaining tab
                  *and* there is no pinned chat tab to land on. With the
                  chat tab (WeChat model) every exec session is freely
                  closable, including the last one. */}
              {(chatTab || openSessionIds.length > 1) && (
                <Tooltip content={t("sessionTabBar.closeTab")} variant="plain">
                  <button
                    onClick={(e) => handleClose(e, sessionId)}
                    className={cn(
                      "shrink-0 rounded p-0.5 transition-opacity",
                      isActive ? "opacity-60 hover:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600" : "opacity-0 group-hover:opacity-60 hover:!opacity-100 hover:bg-zinc-300 dark:hover:bg-zinc-600",
                    )}
                  >
                    <X className="h-3 w-3" />
                  </button>
                </Tooltip>
              )}
            </TabItem>
          );
        })}
      </ScrollableTabBar>

      {/* Action buttons. Container shape (`gap-1 pr-2 shrink-0`) mirrors the
          Locate/Save wrapper in FileEditorPanel so that:
          - Button-to-button gap = 4px (gap-1) on both tab bars
          - Right-most button → panel right edge = 8px (pr-2) + 2px (parent
            px-0.5) = 10px on both tab bars
          Previously this container used `px-1 gap-0.5`, which placed the
          🕐 button only 6px from the right edge and left the two +/🕐
          buttons visually closer than their FileEditorPanel cousins. */}
      <div className="flex items-center gap-1 pr-2 shrink-0">
        {/* Chat-history entry (ADR-097) — sits LEFT of the `+` button and only
            while the Chat surface is active. Wraps in the same `relative
            inline-flex` + `h-6 w-6` geometry as the `+`/🕐 siblings so all
            three tiles share one baseline. */}
        {chatTab?.active && onOpenChatHistory && (
          <div className="relative inline-flex">
            <Tooltip content={t("chatHistory.title")} variant="plain">
              <button
                onClick={onOpenChatHistory}
                aria-label={t("chatHistory.title")}
                className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
              >
                <OutlineChatHistoryIcon size={14} />
              </button>
            </Tooltip>
          </div>
        )}

        {/* New session button. Wrapped in `relative inline-flex` to exactly
            match the +/🕐 siblings below — without the parity, the un-wrapped
            Tooltip wrapper (itself `relative inline-flex`) sits at a slightly
            different baseline than a block-level wrapper, and items-center
            can't fully cancel that micro-offset, leaving the + button visibly
            lower than the 🕐 button. */}
        <div className="relative inline-flex">
          <Tooltip content={t("sessionTabBar.newConversation")} variant="plain">
            <button
              onClick={handleNew}
              className="inline-flex items-center justify-center rounded h-6 w-6 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300 transition-colors"
            >
              <Plus className="h-3.5 w-3.5 shrink-0" />
            </button>
          </Tooltip>
        </div>

        {/* Session list dropdown
            The outer wrapper must stay `relative` so the absolute-positioned
            SessionListDropdown anchors here. We add `inline-flex` to match
            the wrapper that wrapping Tooltip provides for the + button
            (`<div class="relative inline-flex">`) — without this parity, the
            block-level wrapper has subtly different line-height / baseline
            behavior, which `items-center` cannot fully cancel and causes the
            🕐 button to sit a hair higher than the + button. */}
        <div className="relative inline-flex">
          <Tooltip content={t("sessionTabBar.sessionHistory")} variant="plain">
            <button
              onClick={() => setListOpen(!listOpen)}
              className={cn(
                "inline-flex items-center justify-center rounded h-6 w-6 transition-colors",
                listOpen
                  ? "text-[var(--color-accent)] bg-zinc-200 dark:bg-zinc-700"
                  : "text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300",
              )}
            >
              <Clock className="h-3.5 w-3.5" />
            </button>
          </Tooltip>

          {listOpen && (
            <SessionListDropdown
              agentId={agentId}
              activeSessionId={activeSessionId ?? undefined}
              onClose={() => setListOpen(false)}
              // Opening a session from history while the chat surface is on
              // screen flips the strip to the exec surface (same rule as an
              // exec tab click).
              onOpened={showExecSurface}
            />
          )}
        </div>
      </div>

      {/* Close confirmation dialog for looping sessions */}
      {closingSessionId && (
        <div className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay" onClick={() => setClosingSessionId(null)}>
          <div
            className="mx-4 w-full max-w-sm rounded-md border border-border-outer bg-modal-surface p-5 shadow-xl"
            onClick={(e) => e.stopPropagation()}
          >
            <div className="flex items-start gap-3">
              <div className="flex h-9 w-9 shrink-0 items-center justify-center rounded-full bg-amber-100 dark:bg-amber-900/30">
                <TriangleAlert className="h-5 w-5 text-amber-600 dark:text-amber-400" />
              </div>
              <div className="flex-1">
                <h3 className="text-sm font-medium text-text ">
                  {t("sessionTabBar.llmReasoning")}
                </h3>
                <p className="mt-1 text-xs text-text-tertiary ">
                  {t("sessionTabBar.closeWarning")}
                </p>
              </div>
            </div>
            <div className="mt-4 flex justify-end gap-2">
              <button
                onClick={() => setClosingSessionId(null)}
                className="rounded btn-solid px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.cancel")}
              </button>
              <button
                onClick={confirmClose}
                className="rounded btn-accent px-3 py-1.5 text-xs"
              >
                {t("sessionTabBar.confirmClose")}
              </button>
            </div>
          </div>
        </div>
      )}

      {/* Session tab right-click context menu — uses global .context-menu classes */}
      <ContextMenu<{ sessionId: string }>
        isOpen={tabMenu.isOpen}
        menuProps={tabMenu.menuProps}
        items={tabMenuItems}
        payload={tabMenu.payload}
        selectionAtOpen={tabMenu.selectionAtOpen}
        onClose={tabMenu.close}
      />
    </div>
  );
}
