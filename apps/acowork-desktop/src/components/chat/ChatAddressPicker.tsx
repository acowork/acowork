/**
 * ChatAddressPicker — the second level of the `@` (ADR-106 D7).
 *
 * The first level answers "which agent"; this one answers "which of its runs".
 * The rows come from `GET …/sessions` with `chat_session_id` / `surface`, which
 * is the same paging path the session-history menu uses, narrowed by the
 * Runtime's own candidate rule (ADR-106 D7): what the server hands back IS what
 * this chat may address, so the picker never recomputes a permission it cannot
 * see. No window and no row cap — the human pages and searches as far back as
 * they want instead of wondering why a session they remember is missing.
 *
 * Two rules the list must keep:
 * - **"new task" is always the first row.** D6's cheap escape exists because a
 *   question must be answerable in one click; a picker that buried the escape
 *   below the suggestions would be pressing the human back into the machine's
 *   guess.
 * - **picking nothing is still an answer.** Escape and the last row both mean
 *   "no session named", which leaves the message on the ladder's lower levels —
 *   where the Router may answer it as chat instead of routing it. An address is
 *   an instruction, so the absence of one has to stay reachable.
 *
 * Keyboard lives here rather than in the host: the list takes focus on open and
 * owns arrows / Enter / Escape, so both surfaces (the group's `@` popup and the
 * 1:1 composer's address button) drive it the same way and neither re-implements
 * the traversal.
 */

import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type RefObject,
} from "react";
import { ChevronLeft, ChevronRight, Loader2, Plus, Search } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import { useAgentStore } from "../../stores/agentStore";
import { chatListKey } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { isProcessing } from "../../lib/types";
import { listRowSelected } from "../../lib/ui-styles";
import type { ChatSurface, SessionInfo } from "../../lib/types";
import { FilledBrainIcon, OutlineBrainIcon } from "../common/BrainIcon";
import { StyledInput } from "../common/StyledInput";
import { formatRelativeTime } from "./SessionTabBar";

/** What the human decided. `{}` = no address at all (the ladder continues). */
export interface AddressPick {
  sessionId?: string;
  forceNew?: boolean;
  /** Display name of the named session, as shown in the list - the chip that
   *  records the address on the composer needs the human's own words, not an
   *  id. */
  label?: string;
}

/** The human's decision, minus "nothing". */
export type AddressChoice = NonNullable<AddressPick> & { sessionId?: string };

interface ChatAddressPickerProps {
  /** Whose runs these are: the agent instance whose session list is shown, and
   *  the agent the address is finally delivered to. */
  instanceId: string;
  /** Which conversation the human is writing in. Part of the address, not a
   *  filter the client could invent: the Runtime narrows the listing by it, so
   *  a non-participant is refused rather than shown somebody else's work. */
  chatSessionId: string;
  /** `group` for a multi-party chat; a 1:1 leaves it `direct` (the default).
   *  The two have different rules, so the surface is stated, not inferred. */
  surface?: ChatSurface;
  onPick: (pick: AddressPick) => void;
  onClose: () => void;
  /** Shown as the list's subject on a multi-party surface, where "the sessions"
   *  must not read as the group's whole session list. */
  agentName?: string;
  className?: string;
  /** The control that opened the list. Pointer-downs on it are exempt from the
   *  outside-click close, so a toggle button can still close what it opened
   *  (closing on its mousedown and re-opening on its click would fight). */
  anchorRef?: RefObject<HTMLElement | null>;
}

/** One line of the list. `session` rows carry the session; the two fixed rows
 *  are the escape (first) and the refusal to name anything (last). */
type Row =
  | { kind: "new" }
  | { kind: "session"; session: SessionInfo }
  | { kind: "none" };

const EMPTY_SESSIONS: SessionInfo[] = [];

export function ChatAddressPicker({
  instanceId,
  chatSessionId,
  surface = "direct",
  onPick,
  onClose,
  agentName,
  className,
  anchorRef,
}: ChatAddressPickerProps) {
  const { t } = useTranslation();
  // This chat's own page of addressable sessions — the sidebar's list of every
  // session the agent has is a different question, and sharing it would let
  // opening a picker move the sidebar under the human.
  const listKey = chatListKey(instanceId, surface, chatSessionId);
  const storage = useAgentStore((s) => s.chatSessionLists[listKey]);
  const fetchChatSessions = useAgentStore((s) => s.fetchChatSessions);
  const sessions = storage?.sessions ?? EMPTY_SESSIONS;
  const currentPage = storage?.pagination.currentPage ?? 1;
  const totalPages = storage?.pagination.totalPages ?? 1;
  const loading = storage?.isLoading ?? false;
  /** The listing read failed — not "this chat has no tasks yet". */
  const failed = storage?.error ?? false;
  /** Whether a task is still working. The only live source of that is the MQTT
   *  `session_state` push, which lands in `chatStore` for every session this
   *  window has seen (`fetchSessions`' ADR-014 pass cannot help: the session
   *  listing carries no status field). A session with no entry here was never
   *  reported busy, which is idle — the outline mark, never an invented
   *  breath. Subscribing rather than reading `getState()` per row is what keeps
   *  the mark live while the popup stays open. */
  const sessionStates = useChatStore(
    (s) => s.agentStates[instanceId]?.sessionStates,
  );
  const [query, setQuery] = useState("");
  const [active, setActive] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);
  const inputRef = useRef<HTMLInputElement>(null);

  useEffect(() => {
    // Scoped per chat, so nothing else in the app has fetched it: the first
    // open of this chat's picker asks for the first page.
    if (!useAgentStore.getState().chatSessionLists[listKey]) {
      void fetchChatSessions(instanceId, chatSessionId, surface, 1);
    }
  }, [listKey, instanceId, chatSessionId, surface, fetchChatSessions]);

  // The popup opens in its search box: the first gesture here is typing a
  // filter, so the caret has to start where that happens. Keyboard traversal
  // still belongs to the list — arrows / Enter / Escape bubble from the input up
  // to `listRef`'s handler, so the two do not fight. The composer keeps its draft
  // in state, so leaving the textarea costs the human nothing.
  useEffect(() => {
    inputRef.current?.focus();
  }, []);

  // A popup only its opener can dismiss does not read as a popup: pointer-down
  // anywhere outside closes it, the same `mousedown` + `contains` shape every
  // other popover in this app uses (ChatPanel's menus, `useContextMenu`).
  useEffect(() => {
    const onMouseDown = (e: MouseEvent) => {
      const target = e.target as Node;
      if (listRef.current?.contains(target)) return;
      if (anchorRef?.current?.contains(target)) return;
      onClose();
    };
    document.addEventListener("mousedown", onMouseDown);
    return () => document.removeEventListener("mousedown", onMouseDown);
  }, [onClose, anchorRef]);

  // Search filters the page that is loaded, exactly like the session-history
  // menu does — server-side search would be a second query path for one list.
  const matches = useMemo(() => {
    const q = query.trim().toLowerCase();
    return q
      ? sessions.filter((s) => (s.title ?? "").toLowerCase().includes(q))
      : sessions;
  }, [sessions, query]);

  const rows: Row[] = [
    { kind: "new" },
    ...matches.map((session) => ({ kind: "session" as const, session })),
    ...(sessions.length > 0 ? [{ kind: "none" as const }] : []),
  ];

  // A new page or query is a new list: the highlight must not stay on a row the
  // human can no longer see.
  useEffect(() => {
    setActive(0);
  }, [currentPage, query]);

  const choose = useCallback(
    (row: Row) => {
      if (row.kind === "new") onPick({ forceNew: true });
      else if (row.kind === "none") onPick({});
      else
        onPick({
          sessionId: row.session.session_id,
          label: row.session.title ?? undefined,
        });
    },
    [onPick],
  );

  const pickAt = (index: number) => {
    const row = rows[index];
    if (row) choose(row);
  };

  return (
    <div
      ref={listRef}
      role="listbox"
      tabIndex={-1}
      aria-label={t("chatView.address.label")}
      data-testid="chat-address-picker"
      onKeyDown={(e) => {
        if (e.key === "ArrowDown") {
          e.preventDefault();
          setActive((i) => (rows.length === 0 ? 0 : (i + 1) % rows.length));
          return;
        }
        if (e.key === "ArrowUp") {
          e.preventDefault();
          setActive((i) =>
            rows.length === 0 ? 0 : (i - 1 + rows.length) % rows.length,
          );
          return;
        }
        if (e.key === "Enter" || e.key === "Tab") {
          e.preventDefault();
          pickAt(Math.min(active, rows.length - 1));
          return;
        }
        if (e.key === "Escape") {
          e.preventDefault();
          onClose();
        }
      }}
      className={cn(
        // The popup shell both surfaces share: same width, same hairline, so a
        // `@`'s second level looks like its first one.
        "w-72 overflow-hidden rounded-lg border border-border-outer bg-modal-surface shadow-xl outline-none",
        className,
      )}
    >
      <div className="px-4 pb-1 pt-2.5 text-[0.7rem] text-text-tertiary">
        {agentName
          ? t("chatView.address.subject", { name: agentName })
          : t("chatView.address.label")}
      </div>
      <div className="border-b border-border-divider px-4 py-2">
        <div className="relative">
          <Search className="pointer-events-none absolute left-2 top-1/2 h-3 w-3 -translate-y-1/2 text-text-tertiary" />
          <StyledInput
            ref={inputRef}
            type="text"
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder={t("chatView.address.search")}
            aria-label={t("chatView.address.search")}
            className="pl-7"
          />
        </div>
      </div>
      {/* The list region is inset from the shell so a row's highlight wash
          floats inside the hairline instead of bleeding to both edges — the
          `titleBarMenuPanel` grammar. `role="none"` keeps the options owned by
          the `listbox` above. */}
      <div role="none" className="px-1 pb-1">
        {loading && sessions.length === 0 ? (
          <div className="flex items-center gap-2 px-3 py-1.5 text-xs text-text-tertiary">
            <Loader2 size={14} className="animate-spin" />
            {t("chatView.block.loading")}
          </div>
        ) : (
          rows.map((row, i) => {
            const label =
              row.kind === "new"
                ? t("chatView.block.newTask")
                : row.kind === "none"
                  ? t("chatView.address.noPreference")
                  : (row.session.title ?? t("chatView.address.untitled"));
            /** The brain pair from the session-history list (design 27 §6.1):
             *  solid + breathing while the run is working, outline when it is
             *  settled. No accent arm here — the cursor row already wears the
             *  accent wash, and the picker has no "which task is on screen".
             *  The words ride the icon so a screen reader gets what the glyph
             *  says. */
            const working =
              row.kind === "session" &&
              isProcessing(sessionStates?.[row.session.session_id]?.sessionStatus);
            return (
              <button
                key={row.kind === "session" ? row.session.session_id : row.kind}
                type="button"
                role="option"
                aria-selected={i === active}
                data-address-index={i}
                onClick={() => choose(row)}
                onMouseEnter={() => setActive(i)}
                className={
                  // The cursor row wears the app-wide accent tint (the same
                  // family the session-history list uses), never the neutral
                  // `--color-nav-item-hover`, which is reserved for icon
                  // controls.
                  "flex w-full items-center gap-2 rounded px-3 py-1.5 text-left text-xs " +
                  (i === active ? `${listRowSelected} text-text` : "text-text-secondary")
                }
              >
                {row.kind === "new" ? <Plus size={14} className="shrink-0" /> : null}
                {row.kind === "session" ? (
                  <>
                    {working ? (
                      <FilledBrainIcon className="h-3.5 w-3.5 shrink-0 animate-pulse text-text-secondary" />
                    ) : (
                      <OutlineBrainIcon className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                    )}
                    <span className="sr-only">
                      {t(working ? "chatView.address.statusWorking" : "chatView.address.statusIdle")}
                    </span>
                  </>
                ) : null}
                <span className="min-w-0 flex-1 truncate">{label}</span>
                {row.kind === "session" ? (
                  <span className="shrink-0 text-[0.7rem] text-text-tertiary">
                    {formatRelativeTime(row.session.created_at, t)}
                  </span>
                ) : null}
              </button>
            );
          })
        )}
      </div>
      {sessions.length === 0 && !loading && failed ? (
        <div className="flex items-center gap-1.5 px-4 py-1.5 text-[0.7rem] text-text-tertiary">
          {t("chatView.address.failed")}
          <button
            type="button"
            onClick={() =>
              void fetchChatSessions(instanceId, chatSessionId, surface, 1)
            }
            className="rounded px-1 text-brand-primary hover:bg-zinc-100 dark:hover:bg-zinc-800"
          >
            {t("common.retry")}
          </button>
        </div>
      ) : sessions.length === 0 && !loading ? (
        <div className="px-4 py-1.5 text-[0.7rem] text-text-tertiary">
          {t("chatView.address.empty")}
        </div>
      ) : null}
      {/* Pager — same chrome as the session-history menu: the human decides when
          to walk further back instead of a threshold they cannot see. */}
      {totalPages > 1 && (
        <div className="flex items-center justify-between border-t border-border-divider px-3 py-1.5">
          <button
            type="button"
            onClick={() =>
              void fetchChatSessions(instanceId, chatSessionId, surface, currentPage - 1)
            }
            disabled={currentPage <= 1 || loading}
            aria-label={t("chatView.address.prevPage")}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 dark:hover:bg-zinc-800"
          >
            <ChevronLeft className="h-3.5 w-3.5" />
          </button>
          <span className="text-11 text-text-tertiary">
            {t("chatView.address.pagerOf", {
              current: currentPage,
              total: totalPages,
            })}
          </span>
          <button
            type="button"
            onClick={() =>
              void fetchChatSessions(instanceId, chatSessionId, surface, currentPage + 1)
            }
            disabled={currentPage >= totalPages || loading}
            aria-label={t("chatView.address.nextPage")}
            className="inline-flex items-center rounded-md px-1.5 py-0.5 text-text-tertiary hover:bg-zinc-100 disabled:opacity-30 dark:hover:bg-zinc-800"
          >
            <ChevronRight className="h-3.5 w-3.5" />
          </button>
        </div>
      )}
    </div>
  );
}
