/**
 * ChatAddressPicker — the second level of the `@` (ADR-106 D7).
 *
 * The first level answers "which agent"; this one answers "which of its runs".
 * It is deliberately not a session browser: the rows come from the Runtime's
 * candidate filter (`GET …/chat-candidates`), which is the permission that says
 * what this human may address in THIS chat — my runs in a group, this chat's
 * bound sessions plus my own hand-started ones in a 1:1, never terminal work.
 * A picker built off any other list would offer a session the turn could not
 * legally be delivered to.
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
  useRef,
  useState,
  type RefObject,
} from "react";
import { Loader2, Plus } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import {
  fetchChatCandidates,
  type CandidateSurface,
  type ChatCandidate,
} from "../../lib/chatCandidates";

/** What the human decided. `{}` = no address at all (the ladder continues). */
export interface AddressPick {
  sessionId?: string;
  forceNew?: boolean;
  /** Display name of the named session - the chip that records the address on
   *  the composer needs the human's own words, not an id. */
  label?: string;
}

/** The human's decision, minus "nothing". */
export type AddressChoice = NonNullable<AddressPick> & { sessionId?: string };

interface ChatAddressPickerProps {
  /** Whose runs these are. The address rides the wire per agent (D7's
   *  `target_sessions`), so the picker must know the instance it is naming. */
  instanceId: string;
  /** Part of the address, not a filter: the candidate set is per chat. */
  chatSessionId: string;
  surface: CandidateSurface;
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

/** One line of the list. `session` rows carry the candidate; the two fixed rows
 *  are the escape (first) and the refusal to name anything (last). */
type Row =
  | { kind: "new" }
  | { kind: "session"; candidate: ChatCandidate }
  | { kind: "none" };

const STATUS_KEYS: Record<string, string> = {
  working: "chatView.address.statusWorking",
  idle: "chatView.address.statusIdle",
  errored: "chatView.address.statusErrored",
  closed: "chatView.address.statusIdle",
};

export function ChatAddressPicker({
  instanceId,
  chatSessionId,
  surface,
  onPick,
  onClose,
  agentName,
  className,
  anchorRef,
}: ChatAddressPickerProps) {
  const { t } = useTranslation();
  const [candidates, setCandidates] = useState<ChatCandidate[] | null>(null);
  const [failed, setFailed] = useState(false);
  const [active, setActive] = useState(0);
  const [attempt, setAttempt] = useState(0);
  const listRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    const ac = new AbortController();
    let alive = true;
    setFailed(false);
    setCandidates(null);
    fetchChatCandidates(instanceId, chatSessionId, surface, { signal: ac.signal })
      .then((rows) => {
        if (alive) setCandidates(rows);
      })
      .catch(() => {
        if (alive && !ac.signal.aborted) setFailed(true);
      });
    return () => {
      alive = false;
      ac.abort();
    };
  }, [instanceId, chatSessionId, surface, attempt]);

  // The list owns the keyboard once it is open, so focus it — the composer keeps
  // the draft in state, so leaving the textarea costs the human nothing.
  useEffect(() => {
    listRef.current?.focus();
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

  const rows: Row[] = [
    { kind: "new" },
    ...(candidates ?? []).map((candidate) => ({ kind: "session" as const, candidate })),
    ...(candidates && candidates.length > 0 ? [{ kind: "none" as const }] : []),
  ];

  const choose = useCallback(
    (row: Row) => {
      if (row.kind === "new") onPick({ forceNew: true });
      else if (row.kind === "none") onPick({});
      else onPick({ sessionId: row.candidate.session_id, label: row.candidate.label });
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
      <div className="px-2 pb-1 pt-1.5 text-[0.7rem] text-text-tertiary">
        {agentName
          ? t("chatView.address.subject", { name: agentName })
          : t("chatView.address.label")}
      </div>
      {failed ? (
        <button
          type="button"
          role="option"
          aria-selected={false}
          onClick={() => setAttempt((a) => a + 1)}
          className="flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs text-text-secondary hover:bg-nav-item-hover"
        >
          <span className="min-w-0 flex-1 truncate">{t("chatView.address.failed")}</span>
          <span className="shrink-0 text-[0.7rem] text-accent">{t("chatView.block.retry")}</span>
        </button>
      ) : null}
      {!failed && candidates === null ? (
        <div className="flex items-center gap-2 px-2 py-1.5 text-xs text-text-tertiary">
          <Loader2 size={14} className="animate-spin" />
          {t("chatView.block.loading")}
        </div>
      ) : null}
      {failed || candidates === null
        ? null
        : rows.map((row, i) => {
            const label =
              row.kind === "new"
                ? t("chatView.block.newTask")
                : row.kind === "none"
                  ? t("chatView.address.noPreference")
                  : row.candidate.label;
            return (
              <button
                key={row.kind === "session" ? row.candidate.session_id : row.kind}
                type="button"
                role="option"
                aria-selected={i === active}
                data-address-index={i}
                onClick={() => choose(row)}
                onMouseEnter={() => setActive(i)}
                className={
                  "flex w-full items-center gap-2 px-2 py-1.5 text-left text-xs " +
                  (i === active ? "bg-nav-item-hover text-text" : "text-text-secondary")
                }
              >
                {row.kind === "new" ? <Plus size={14} className="shrink-0" /> : null}
                <span className="min-w-0 flex-1 truncate">{label}</span>
                {row.kind === "session" ? (
                  <span className="shrink-0 text-[0.7rem] text-text-tertiary">
                    {t(STATUS_KEYS[row.candidate.status] ?? "chatView.address.statusIdle")}
                  </span>
                ) : null}
              </button>
            );
          })}
      {candidates !== null && candidates.length === 0 ? (
        <div className="px-2 py-1.5 text-[0.7rem] text-text-tertiary">
          {t("chatView.address.empty")}
        </div>
      ) : null}
    </div>
  );
}
