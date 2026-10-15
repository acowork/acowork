// Design 27 Phase 3c: "Agent 页面 Chat history 入口" — a companion to the
// Session history dropdown (SessionTabBar's 🕐 button). Chat sessions are
// the IM-style message surface (project group chats + agent↔agent channels)
// stored by the user service and reached through the Gateway's
// `/api/chat-sessions` reverse proxy; this view is read-only — opening an
// execution session (the 🕐 path) stays the interactive surface.
//
// Local mode has no user service and therefore no chat sessions at all:
// the list fetch 503s and the dropdown shows the empty state.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { getGatewayUrl } from "../../lib/config";
import { cn } from "../../lib/utils";
import { listRowHover } from "../../lib/ui-styles";
import { ArrowLeft, MessagesSquare, X } from "lucide-react";

// ── Shapes (mirror acowork-user chat_sessions_api responses) ───────────

interface ChatSessionRow {
  session_id: string;
  kind: string;
  title: string;
  participants: string[];
  project_id?: string;
  repo?: string;
  created_at: number;
  last_active_at: number;
  message_count: number;
}

interface ChatMessageRow {
  seq: number;
  ts: number;
  from: string;
  kind: string;
  body: string;
  to: string;
  meta?: {
    message_id?: string;
    conversation_id?: string;
    in_reply_to?: string;
    status?: string;
    repo?: string;
  };
}

interface MessagePage {
  messages: ChatMessageRow[];
  total: number;
}

const PAGE = 50;

// ── Small helpers ──────────────────────────────────────────────────────

function relTime(tsSec: number, t: (k: string, o?: Record<string, unknown>) => string): string {
  const diff = Math.floor(Date.now() / 1000) - tsSec;
  if (diff < 60) return t("time.justNow");
  if (diff < 3600) return t("time.minutesAgo", { count: Math.floor(diff / 60) });
  if (diff < 86400) return t("time.hoursAgo", { count: Math.floor(diff / 3600) });
  if (diff < 30 * 86400) return t("time.daysAgo", { count: Math.floor(diff / 86400) });
  return new Date(tsSec * 1000).toLocaleDateString("en", { month: "short", day: "numeric" });
}

/** Participant ids are UUIDs; show enough of the prefix to tell actors
 *  apart without flooding the row. */
function shortId(id: string): string {
  return id.length > 10 ? id.slice(0, 8) + "…" : id;
}

// ── Dropdown ───────────────────────────────────────────────────────────

interface ChatHistoryDropdownProps {
  /** The agent's instance id — chat sessions address participants by it
   *  (ADR-073), so it doubles as the list query key. */
  agentId: string;
  onClose: () => void;
}

export function ChatHistoryDropdown({ agentId, onClose }: ChatHistoryDropdownProps) {
  const { t } = useTranslation();
  const ref = useRef<HTMLDivElement>(null);
  const [sessions, setSessions] = useState<ChatSessionRow[] | null>(null);
  const [openSession, setOpenSession] = useState<ChatSessionRow | null>(null);

  useEffect(() => {
    let cancelled = false;
    void (async () => {
      try {
        const resp = await fetch(`${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(agentId)}`);
        if (!resp.ok) {
          if (!cancelled) setSessions([]);
          return;
        }
        const data = (await resp.json()) as ChatSessionRow[];
        if (!cancelled) setSessions(data);
      } catch {
        if (!cancelled) setSessions([]);
      }
    })();
    return () => {
      cancelled = true;
    };
  }, [agentId]);

  useEffect(() => {
    const handler = (e: MouseEvent) => {
      if (ref.current && !ref.current.contains(e.target as Node)) onClose();
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [onClose]);

  return (
    <div
      ref={ref}
      className="absolute right-0 top-full mt-1 w-72 rounded-md border border-border-outer bg-modal-surface shadow-lg z-50"
    >
      <div className="border-b border-border-divider px-3 py-1.5 text-11 text-text-tertiary">
        {t("chatHistory.title")}
      </div>
      <div className="max-h-80 overflow-y-auto py-1">
        {sessions === null && (
          <div className="px-3 py-4 text-center text-xs text-text-tertiary">…</div>
        )}
        {sessions !== null && sessions.length === 0 && (
          <div className="px-3 py-4 text-center text-xs text-text-tertiary">
            {t("chatHistory.empty")}
          </div>
        )}
        {sessions?.map((s) => (
          <button
            key={s.session_id}
            onClick={() => setOpenSession(s)}
            className={cn("flex w-full flex-col gap-0.5 px-3 py-2 text-left", listRowHover)}
          >
            <div className="flex items-center gap-2 min-w-0">
              <MessagesSquare className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
              <span className="truncate text-xs text-text-primary">
                {s.title || t("chatHistory.untitled")}
              </span>
            </div>
            <div className="ml-5.5 flex items-center gap-2 text-10 text-text-tertiary">
              <span>{t(`chatHistory.kind.${s.kind}`)}</span>
              <span>·</span>
              <span>{relTime(s.last_active_at, t)}</span>
              <span>·</span>
              <span>{s.message_count} msg</span>
            </div>
          </button>
        ))}
      </div>

      {openSession && (
        <ChatMessagesModal
          session={openSession}
          onBack={() => setOpenSession(null)}
          onClose={onClose}
        />
      )}
    </div>
  );
}

// ── Message modal ──────────────────────────────────────────────────────

interface ChatMessagesModalProps {
  session: ChatSessionRow;
  onBack: () => void;
  onClose: () => void;
}

function ChatMessagesModal({ session, onBack, onClose }: ChatMessagesModalProps) {
  const { t } = useTranslation();
  // `pages` accumulates newest-first windows; offset counts back from the
  // newest message (user service read semantics).
  const [messages, setMessages] = useState<ChatMessageRow[] | null>(null);
  const [offset, setOffset] = useState(0);
  const [total, setTotal] = useState(0);
  const [loading, setLoading] = useState(false);

  const load = async (from: number) => {
    setLoading(true);
    try {
      const resp = await fetch(
        `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(session.session_id)}/messages` +
          `?offset=${from}&limit=${PAGE}`,
      );
      if (resp.ok) {
        const page = (await resp.json()) as MessagePage;
        // newest-first pages, rendered oldest-first within the window
        setMessages((prev) => [...page.messages].reverse().concat(prev ?? []));
        setTotal(page.total);
        setOffset(from + page.messages.length);
      }
    } catch {
      // keep whatever is already rendered; the button stays for a retry
    } finally {
      setLoading(false);
    }
  };

  useEffect(() => {
    void load(0);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [session.session_id]);

  const hasMore = messages === null || offset < total;

  return (
    <div
      className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay"
      onClick={onClose}
    >
      <div
        className="mx-4 flex h-[70vh] w-full max-w-2xl flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
        onClick={(e) => e.stopPropagation()}
      >
        <div className="flex items-center gap-2 border-b border-border-divider px-3 py-2">
          <button
            onClick={onBack}
            className="rounded p-1 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
          >
            <ArrowLeft className="h-4 w-4" />
          </button>
          <div className="min-w-0 flex-1">
            <div className="truncate text-sm text-text-primary">{session.title || t("chatHistory.untitled")}</div>
            <div className="text-10 text-text-tertiary">
              {t(`chatHistory.kind.${session.kind}`)} · {total} {t("chatHistory.messages")}
            </div>
          </div>
          <button
            onClick={onClose}
            className="rounded p-1 text-text-tertiary hover:bg-zinc-200 hover:text-zinc-600 dark:hover:bg-zinc-700 dark:hover:text-zinc-300"
          >
            <X className="h-4 w-4" />
          </button>
        </div>

        <div className="flex-1 overflow-y-auto px-3 py-2">
          {messages === null && (
            <div className="py-6 text-center text-xs text-text-tertiary">…</div>
          )}
          {messages !== null && messages.length === 0 && (
            <div className="py-6 text-center text-xs text-text-tertiary">{t("chatHistory.noMessages")}</div>
          )}
          {messages !== null && hasMore && (
            <div className="py-2 text-center">
              <button
                onClick={() => void load(offset)}
                disabled={loading}
                className="rounded btn-solid px-3 py-1 text-xs disabled:opacity-50"
              >
                {t("chatHistory.loadEarlier")}
              </button>
            </div>
          )}
          {messages?.map((m) => (
            <div key={m.seq} className="border-b border-border-divider/50 py-2 last:border-0">
              <div className="mb-1 flex items-center gap-2 text-10 text-text-tertiary">
                <span className="font-medium">{shortId(m.from)}</span>
                {m.kind !== "user_message" && (
                  <span
                    className={cn(
                      "rounded px-1.5 py-0.5",
                      m.kind === "intent_request"
                        ? "bg-blue-500/10 text-blue-500"
                        : "bg-emerald-500/10 text-emerald-600",
                    )}
                  >
                    {m.kind === "intent_request"
                      ? `→ ${shortId(m.to)}`
                      : `⇐ ${shortId(m.to)}${m.meta?.status && m.meta.status !== "completed" ? ` · ${m.meta.status}` : ""}`}
                  </span>
                )}
                <span className="ml-auto">{relTime(m.ts, t)}</span>
              </div>
              <div className="whitespace-pre-wrap break-words text-xs text-text-primary">{m.body}</div>
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}
