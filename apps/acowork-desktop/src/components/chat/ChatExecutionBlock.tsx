/**
 * ChatExecutionBlock — one execution SESSION's worth of rounds, drawn as one
 * collapsible block (ADR-106 D3/D4/D8/D9; the presentation contract in design
 * 27 §5.5).
 *
 * The chat holds no copy of the work. Its transcript carries one bodyless
 * reference row per round, and the layout layer (`chatTranscriptGroups`) folds a
 * session's rows into ONE block: drawing a card per row turned a long-lived run
 * into a stack of near-identical boxes, which is neither the transcript's nor the
 * execution surface's grammar.
 *
 * The visual language is copied from the execution surface's `ExploreBlock`,
 * because a run of rounds is the same kind of object as a run of tool calls: an
 * avatar and name above, a `bg-zinc-100/60` pill as the collapse header, a
 * `border-l-2` list under it, one row per item with a status glyph and a
 * chevron, and the pair's content in two stacked panels under the open row. The
 * only invention here is what a row pairs: `user` / `assistant` instead of
 * `tool_call` / `tool_result`, plus the session's live state next to the count.
 *
 * Three rules this component owns:
 * - "in progress" is positive evidence only — a `pending` projection read (D9).
 *   `session_state` refines what that looks like; it never substitutes for it.
 * - the card a round is parked on lives HERE, in the block (D8). Answering it
 *   posts to the execution session's own write entry, where the owner check
 *   already runs; no chat row is written for the decision.
 * - a hole is drawn as a hole: a round that cannot be read says so, instead of
 *   the transcript inventing an outcome for it (D3/D5).
 */

import { memo, useEffect, useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  Loader2,
  MessageCircleQuestion,
  Pause,
  Search,
  ShieldCheck,
  Wrench,
  X,
  Check,
  Send,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";
import type { BlockCard, ChatBlockEntry } from "../../lib/chatBlocks";
import type { RunBlock } from "./chatTranscriptGroups";
import { useChatBlockStore, useChatBlocks } from "../../stores/chatBlockStore";
import { useChatStore } from "../../stores/chatStore";
import { useChatSessionStore } from "../../stores/chatSessionStore";
import { useAgentStore } from "../../stores/agentStore";
import { useSessionTitle } from "../../lib/useSessionTitle";
import { sendAnswer, sendApproval } from "../../lib/session-control";
import { deriveSessionTitle } from "../../lib/sessionConfigMapper";
import { getProcessingPhase, isProcessing, type SessionStatus } from "../../lib/types";
import { AgentAvatar } from "../common/AgentAvatar";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { ViewRunButton } from "./ViewRunButton";
import { ChatMarkdown } from "./ChatMarkdown";

/** Same 90% / 80% scale as `ExploreBlock`, so a chat run and an exec run read at
 *  one size. */
const BLOCK_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
const BLOCK_DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

/** How many of a session's rounds stay visible when the block is opened. The
 *  older ones are one click away inside the block: the list is already a scroll
 *  container, and a long run should not push the rest of the transcript down. */
const RECENT_ROUNDS = 2;
/** How many more each reveal adds, so "show earlier" never dumps a hundred rows
 *  into a 240px pane at once. */
const REVEAL_STEP = 5;

interface ChatExecutionBlockProps {
  /** The session's folded rounds, in order; the newest is the block's anchor. */
  block: RunBlock;
  /** The Chat this block belongs to — the block read is participation-checked
   *  against it, so it is part of the address, not a filter (D10). */
  chatSessionId: string;
  /** Avatar + name above the block. Adjacent bubbles from the same speaker fold
   *  their header; a block always starts a new turn, so the surfaces pass this
   *  true except where a run follows its own reply. */
  showHeader?: boolean;
  /** Who ran the session, as this surface names them. Falls back to the agent's
   *  own display name. */
  authorName?: string;
  /** Something follows this block in the transcript. This is `ExploreBlock`'s
   *  `hasFollowUpReply` — the ONE condition that auto-collapses: while the run is
   *  the freshest thing on screen it stays open, and once the conversation has
   *  moved past it it folds up. */
  hasFollowUp?: boolean;
  /** Drill into the execution session that owns these rounds. */
  onJumpToExec?: (agentInstanceId: string, execSid: string) => void;
  /** Address the composer to one round (D6 level 1) — what a `pause` card's
   *  "continue" and any follow-up on a finished round ride. */
  onReplyToRound?: (rowId: string, quote: string) => void;
  /** A viewer who is not a round's answerer (`answerable_by`): the question
   *  shows, the buttons do not (D10's read/write split). */
  canAnswer?: boolean;
  /** Group surface only: right-click the author → the panel's "@name" menu. */
  onAuthorContextMenu?: (e: React.MouseEvent) => void;
  /** ADR-106 revision 2026-11-04: the `@` picker's "去续投" located this block
   *  and wants its in-place input focused. A rising edge focuses (and reveals)
   *  the reply box, so the human lands ready to type instead of hunting. */
  focusInput?: boolean;
  className?: string;
}

/** Outcome copy for a settled round. The settled outcomes reuse the turn-tier
 *  notice keys — "how a round ended" reads the same whether it is a status line
 *  or a block header, and a second copy of that copy would drift. */
function statusKey(status: string): string {
  switch (status) {
    case "completed":
      return "chatView.block.completed";
    case "failed":
      return "chatView.notice.failed";
    case "aborted":
    case "interrupted":
      return "chatView.notice.interrupted";
    case "timeout":
      return "chatView.notice.timeout";
    case "waiting":
      return "chatView.notice.waiting";
    case "in_progress":
      return "chatView.block.inProgress";
    case "unknown":
      return "chatView.block.unknown";
    default:
      // "in progress" is a claim that needs evidence (D9): an unrecognised — or
      // missing — status is an unknown outcome, never a running one.
      return "chatView.block.unknown";
  }
}

/** The session's live state, in as few words as the header can carry. This is
 *  `session_state` talk, not the round's: D9's "is this still running" stays a
 *  `pending` read, and this only says what a run that IS in flight is doing. */
const STATE_KEYS: Record<string, string> = {
  idle: "chatView.block.state.idle",
  llm_awaiting_first_chunk: "chatView.block.state.waiting",
  thinking: "chatView.block.state.thinking",
  llm_streaming: "chatView.block.state.streaming",
  tool_executing: "chatView.block.state.toolExecuting",
  waiting_approval: "chatView.block.state.waitingApproval",
  paused: "chatView.block.state.paused",
};

function stateText(
  t: (key: string, opts?: Record<string, unknown>) => string,
  status: SessionStatus | null,
): string {
  if (!status) return "";
  return t(STATE_KEYS[status.status] ?? "chatView.block.state.idle", {
    defaultValue: getProcessingPhase(status),
  });
}

/** One-line summary for a round row, the way a tool row shows its arguments:
 *  whitespace folded, cut at the width a row can hold. */
function summarize(text: string, max = 60): string {
  const collapsed = text.replace(/\s+/g, " ").trim();
  return collapsed.length > max
    ? `${collapsed.slice(0, max - 1)}…`
    : collapsed;
}

export const ChatExecutionBlock = memo(function ChatExecutionBlock({
  block,
  chatSessionId,
  showHeader,
  authorName,
  hasFollowUp,
  onJumpToExec,
  onReplyToRound,
  canAnswer = true,
  onAuthorContextMenu,
  focusInput = false,
  className,
}: ChatExecutionBlockProps) {
  const { t } = useTranslation();
  const instanceId = block.instanceId;
  const execSid = block.execSessionId;
  const refs = useMemo(() => block.rounds.map((r) => r.ref), [block]);
  const entries = useChatBlocks(chatSessionId, refs);
  const reload = useChatBlockStore((s) => s.reload);
  const meta = useAgentStore((s) => s.agents[instanceId]?.meta);
  const runName = useSessionTitle(instanceId, execSid);
  const sessionStatus = useChatStore(
    (s) =>
      s.agentStates?.[instanceId]?.sessionStates?.[execSid]?.sessionStatus ??
      null,
  );

  // D9: a `pending` read is the ONLY positive evidence a round is still going.
  // `session_state` says what the session is doing, but a block whose read came
  // back terminal is history even if the session has since started another turn.
  // The round that speaks for the block is its NEWEST one — the block is anchored
  // at it, so it is the round the reader is looking at. An older read that never
  // resolves must not keep a finished run looking live: "how did this end" is a
  // question about the newest round, not a vote across the session.
  const newest = block.rounds[block.rounds.length - 1];
  const newestEntry = newest ? entries[newest.ref] : undefined;
  const newestPending = newestEntry?.status === "pending";
  // A round parked on a card is not running — it is waiting for the human, and
  // saying "executing" over an unanswered card is the one thing D8 forbids: the
  // reader was told to wait when the block was in fact waiting for them.
  const parked = newestPending && !!newestEntry?.projection?.card;
  const outcomeStatus = newestPending
    ? parked
      ? "waiting"
      : "in_progress"
    : newestEntry?.status === "ok"
      ? (newestEntry.projection?.status ?? "unknown")
      : // A hole or an outage says what it is (D9): no read is not evidence of a
        // running round, and a missing state word is not evidence of a finished
        // one either.
        "unknown";
  const inProgress = outcomeStatus === "in_progress";
  // A round parked on a decision the human has not answered: the block must be
  // open to be answerable, which is `ExploreBlock`'s pending-approval rule.
  const hasOpenCard = block.rounds.some((r) => {
    const entry = entries[r.ref];
    return entry?.projection?.card && entry.status === "pending";
  });
  // The session's own activity outranks the block's settled rounds: a session
  // that IS busy is the freshest fact on this surface, and the round it is busy
  // with may simply not have reached this Chat as a reference row yet. So the
  // live word wins; the read-derived word is what the header falls back to.
  // What the live state may NOT do is create liveness out of nothing — the
  // spinner and the auto-expand below stay tied to this block's own pending read
  // (D9).
  const shownState =
    (sessionStatus && isProcessing(sessionStatus)
      ? stateText(t, sessionStatus)
      : "") || t(statusKey(outcomeStatus));

  // Rounds beyond the recent window stay in the block, one click away — except
  // the ones the transcript could not draw anywhere else. Two kinds qualify:
  //  * a round that owns this Chat's own question. Its `user_message` row was
  //    folded into the block (one sentence, one place), so windowing the round
  //    away hides the human's own words from the whole transcript and leaves the
  //    visible rows looking like the agent answering questions nobody asked here;
  //  * a round parked on an unanswered card: an answerable thing behind "show
  //    earlier" is not answerable (D8).
  // Rounds the block produced on its own — the human turned to the execution
  // surface, or the session kept iterating — stay behind the reveal, which is
  // what keeps a long run from pushing the transcript down.
  const [revealed, setRevealed] = useState(0);
  const pinned = new Set<number>();
  block.rounds.forEach((round, i) => {
    if (round.showUserLine && round.triggerRow) pinned.add(i);
    const entry = entries[round.ref];
    if (entry?.status === "pending" && entry.projection?.card) pinned.add(i);
  });
  const start = Math.max(
    0,
    block.rounds.length - RECENT_ROUNDS - revealed,
  );
  const shownIndices = block.rounds
    .map((_, i) => i)
    .filter((i) => i >= start || pinned.has(i));
  const shown = shownIndices.map((i) => block.rounds[i]!);
  const hiddenOlder = block.rounds.length - shownIndices.length;

  // `ExploreBlock`'s state machine, on the same conditions: open while it is the
  // freshest thing on screen, fold once the conversation moves past it, and never
  // re-open over a reader who collapsed it by hand.
  const [expanded, setExpanded] = useState(!hasFollowUp);
  const manuallyCollapsed = useRef(false);
  const contentRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if ((inProgress || hasOpenCard) && !manuallyCollapsed.current) {
      setExpanded(true);
    }
  }, [inProgress, hasOpenCard]);

  useEffect(() => {
    if (hasOpenCard) {
      // The answer buttons have to be on screen, not just in the pane.
      manuallyCollapsed.current = false;
      setExpanded(true);
      setTimeout(() => {
        if (contentRef.current) {
          contentRef.current.scrollTop = contentRef.current.scrollHeight;
        }
      }, 0);
    }
  }, [hasOpenCard]);

  useEffect(() => {
    if (hasFollowUp && !inProgress && !hasOpenCard) {
      setExpanded(false);
      manuallyCollapsed.current = false;
    }
  }, [hasFollowUp, inProgress, hasOpenCard]);

  const name =
    runName?.trim() ||
    // The session's own opening question — the same text the optimistic title
    // seed is cut from, so a block and the tab that later shows its session do
    // not name the same run two different ways.
    deriveSessionTitle(
      block.rounds.find((r) => r.triggerRow)?.triggerRow?.body ?? "",
    ) ||
    t("chatView.block.unnamedRun");
  const displayName =
    authorName?.trim() ||
    meta?.display_name ||
    meta?.name ||
    instanceId;
  // The task coordinate (ADR-106 D13): it rides the ROUND, not the utterance, and
  // the header is where a reader looks for "which piece of work is this". A
  // session bound to a task carries the same id on every round, so the first one
  // wins; a round that carries none paints nothing.
  const taskId = block.rounds.find((r) => r.roundRow.meta?.task_id)?.roundRow
    .meta?.task_id;

  return (
    <div className={`px-4 py-1 ${className ?? ""}`}>
      {showHeader && (
        <div className="mb-2 mt-1 flex items-center gap-2">
          <span onContextMenu={onAuthorContextMenu} className="shrink-0">
            <AgentAvatar
              agentId={instanceId}
              displayName={displayName}
              avatarUrl={meta?.avatar ?? null}
              version={meta?.version}
              builtinAvatarId={meta?.builtin_avatar ?? null}
              size={40}
              className="shrink-0"
            />
          </span>
          <div className="flex flex-col">
            <span className="text-xs font-medium text-text-tertiary ">
              {displayName}
            </span>
            {meta?.role && (
              <span className="text-10 leading-tight text-text-tertiary ">
                {meta.role}
              </span>
            )}
          </div>
        </div>
      )}
      <div className="flex items-start gap-2">
        {/* The same 40px gutter the agent bubble uses, so the block and the
            reply under it start on one line. */}
        <div className="w-10 shrink-0" />
        <div className="flex min-w-0 flex-1 flex-col items-start">
          <div className="flex min-w-0 max-w-[var(--content-max-width)] flex-col items-start">
            {/* The drill keeps its own chrome and sits ABOVE the block: it opens
                the execution session, which is a different object from the
                rounds listed inside it. */}
            {onJumpToExec && (
              <ViewRunButton
                runName={runName ?? name}
                onJump={() => onJumpToExec(instanceId, execSid)}
                className="mb-1 inline-flex shrink-0 items-center gap-1 rounded-full bg-zinc-100 px-2 py-0.5 text-10 text-text-tertiary transition-colors hover:bg-[var(--color-accent)]/10 hover:text-[var(--color-accent)] dark:bg-zinc-800"
              />
            )}
            <div className="max-w-full" style={{ fontSize: BLOCK_FONT_SIZE }}>
              <button
                onClick={() => {
                  const next = !expanded;
                  setExpanded(next);
                  manuallyCollapsed.current = next ? false : true;
                }}
                aria-expanded={expanded}
                className="flex w-fit items-center gap-2 rounded-md bg-zinc-100/60 px-2.5 py-1.5 text-text-tertiary transition-colors hover:bg-zinc-200 dark:bg-zinc-800/30  dark:hover:bg-zinc-700"
                style={{ fontSize: BLOCK_FONT_SIZE }}
              >
                <Search className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
                <span className="min-w-0 max-w-[16rem] truncate font-medium text-text-tertiary ">
                  {name}
                </span>
                <span className="text-text-tertiary">
                  ({t("chatView.block.roundCount", { count: block.rounds.length })}
                  {shownState && (
                    <>
                      {" · "}
                      {shownState}
                    </>
                  )}
                  )
                </span>
                {taskId && (
                  <span className="shrink-0 rounded bg-surface/70 px-1.5 py-0.5 text-[0.8em] text-text-secondary">
                    {taskId}
                  </span>
                )}
                {inProgress && (
                  <Loader2 className="h-3 w-3 shrink-0 animate-spin" />
                )}
                {expanded ? (
                  <ChevronDown className="ml-auto h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                ) : (
                  <ChevronRight className="ml-auto h-3.5 w-3.5 shrink-0 text-text-tertiary" />
                )}
              </button>

              {expanded && (
                <OverlayScrollbar
                  ref={contentRef}
                  /** Twice `ExploreBlock`'s cap: that list is one-line tool
                   *  labels, this one unfolds a whole question/answer pair per
                   *  round, so 144px showed barely one row of prose. */
                  maxHeight={288}
                  containerClassName="flex-none ml-2 mt-1 rounded-md border-l-2 border-zinc-300 bg-zinc-100/60 dark:border-zinc-600 dark:bg-zinc-800/30"
                  className="pl-3 pr-2 py-2 overscroll-contain"
                  onViewportWheel={(event) => {
                    if (event.deltaY !== 0) event.stopPropagation();
                  }}
                >
                  <div className="flex flex-col gap-0.5">
                    {hiddenOlder > 0 && (
                      <button
                        type="button"
                        onClick={() => setRevealed((n) => n + REVEAL_STEP)}
                        className="self-start rounded px-1 py-0.5 text-text-tertiary underline underline-offset-2 hover:text-text-secondary"
                      >
                        {t("chatView.block.olderRounds", { count: hiddenOlder })}
                      </button>
                    )}
                    {shown.map((round, i) => (
                      <RoundItem
                        key={round.ref}
                        round={round}
                        /** The row's label is its position in the SESSION's list,
                         *  so a pinned round above the window keeps its own
                         *  number instead of being renumbered by the window. */
                        index={shownIndices[i]!}
                        /** The freshest round of an open block shows its pair;
                         *  earlier ones fold to one line, the way a long run's
                         *  older tool calls do. */
                        defaultOpen={shownIndices[i] === block.rounds.length - 1}
                        chatSessionId={chatSessionId}
                        instanceId={instanceId}
                        execSid={execSid}
                        entry={entries[round.ref]}
                        reload={reload}
                        canAnswer={canAnswer}
                        onReplyToRound={onReplyToRound}
                      />
                    ))}
                  </div>
                </OverlayScrollbar>
              )}
              {/* ADR-106 revision 2026-11-04: reply in place. Continuing a run
                  is answering its freshest round (D6 level 1), and the point of
                  the `@`'s "去续投" is that the human never leaves the block to
                  do it. Rendered only for an answerer; the box itself reveals
                  on hover / focus and refuses to hide while it holds a draft. */}
              {canAnswer && (
                <BlockInlineReply
                  chatSessionId={chatSessionId}
                  rowId={
                    block.rounds[block.rounds.length - 1]?.roundRow.meta
                      ?.message_id ?? ""
                  }
                  focusSignal={focusInput}
                />
              )}
            </div>
          </div>
        </div>
      </div>
    </div>
  );
});

interface RoundItemProps {
  round: RunBlock["rounds"][number];
  /** Position in the session's round list (1-based) — the row's label. */
  index: number;
  defaultOpen: boolean;
  chatSessionId: string;
  instanceId: string;
  execSid: string;
  entry: ChatBlockEntry | undefined;
  reload: (chatId: string, refs: string[]) => void;
  canAnswer: boolean;
  onReplyToRound?: (rowId: string, quote: string) => void;
}

/** One round: a question line that opens into the question/answer pair, in
 *  `ExploreBlock`'s row-plus-two-panels chrome. */
function RoundItem({
  round,
  index,
  defaultOpen,
  chatSessionId,
  instanceId,
  execSid,
  entry,
  reload,
  canAnswer,
  onReplyToRound,
}: RoundItemProps) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(defaultOpen);
  const projection = entry?.projection;
  // An opener round's sentence stands on the transcript as its own bubble —
  // the block's line leads with what the execution session actually received
  // (the router's brief), never with a second copy of the raw text.
  const question =
    (round.showUserLine ? round.triggerRow?.body : "") ||
    (round.questionOnTranscript ? round.roundRow.meta?.assignment_brief ?? "" : "") ||
    projection?.user?.body ||
    "";
  const summary =
    summarize(question) ||
    summarize(round.roundRow.meta?.assignment_brief ?? "") ||
    t("chatView.block.noQuestion");
  const pending = entry?.status === "pending";
  const card = projection?.card ?? null;
  // A row parked on a card is waiting for the human, not executing — the same
  // rule the header follows, because a row that says "running" over the card it
  // is waiting on is the block contradicting its own buttons.
  const status = !pending
    ? entry?.status === "ok"
      ? (projection?.status ?? "unknown")
      : "unknown"
    : card
      ? "waiting"
      : "in_progress";
  const inProgress = status === "in_progress";
  const failed = status === "failed" || status === "timeout";
  const userStopped = status === "aborted" || status === "interrupted";

  // A card the human has not answered yet must be on screen, not behind a
  // closed row — the block opened for it, so the row opens too.
  useEffect(() => {
    if (card && pending) setOpen(true);
  }, [card, pending]);

  return (
    <div className="min-w-0" data-seq={round.roundRow.seq}>
      <div className="flex min-w-0 w-full items-center gap-2 rounded-md bg-zinc-100 px-2.5 py-1.5 text-left transition-colors hover:bg-zinc-200 dark:bg-zinc-700/50 dark:hover:bg-zinc-700">
        <button
          className="flex min-w-0 flex-1 items-center gap-2"
          onClick={() => setOpen((v) => !v)}
        >
          <Wrench className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
          <span className="shrink-0 font-medium text-text-secondary ">
            {t("chatView.block.roundN", { n: index + 1 })}
          </span>
          <span className="min-w-0 flex-1 truncate ml-1 text-left text-text-tertiary ">
            {summary}
          </span>
        </button>
        {inProgress ? (
          <span className="h-3 w-3 shrink-0 animate-pulse rounded-full bg-zinc-300 dark:bg-zinc-500" />
        ) : status === "waiting" ? (
          <Pause className="h-3 w-3 shrink-0 text-amber-500" />
        ) : failed ? (
          <X className="h-3 w-3 shrink-0 text-red-500" />
        ) : userStopped ? (
          <X className="h-3 w-3 shrink-0 text-amber-500" />
        ) : entry?.status === "unreachable" ? (
          <X className="h-3 w-3 shrink-0 text-amber-500" />
        ) : status === "completed" ? (
          <Check className="h-3 w-3 shrink-0" style={{ color: "var(--color-accent)" }} />
        ) : (
          <span className="h-3 w-3 shrink-0 animate-pulse rounded-full bg-zinc-300 dark:bg-zinc-500" />
        )}
        <button onClick={() => setOpen((v) => !v)} aria-label={t("chatView.block.toggleRound")}>
          {open ? (
            <ChevronDown className="h-3 w-3 shrink-0 text-text-tertiary" />
          ) : (
            <ChevronRight className="h-3 w-3 shrink-0 text-text-tertiary" />
          )}
        </button>
      </div>

      {open && (
        <div className="mt-0.5 ml-5 space-y-0.5">
          {/* A round this surface does not own the question for (an `@A @B`
              fan-out, where the other session's block already wears it; or the
              opener round, whose bubble stands on the transcript itself)
              renders the answer alone — one sentence never appears twice. */}
          {round.showUserLine && question && (
            <pre
              data-seq={round.triggerRow?.seq}
              /** The pair wears the transcript's own bubble language: the
               *  human's line takes the highlight wash (what a user message
               *  costs in the list), the answer takes the agent bubble fill.
               *  Inverting them made a reply read as the speaker who is not
               *  the reader. */
              className="rounded bg-[var(--color-accent)]/10 p-2 text-text-secondary whitespace-pre-wrap break-words"
              style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
            >
              {question}
            </pre>
          )}
          <RoundBody
            entry={entry}
            chatSessionId={chatSessionId}
            reload={reload}
            ref_={round.ref}
            status={status}
            failed={failed}
          />
          {card ? (
            <BlockCardActions
              card={card}
              canAnswer={canAnswer}
              instanceId={instanceId}
              execSid={execSid}
              rowId={round.roundRow.meta?.message_id ?? ""}
              onReplyToRound={onReplyToRound}
            />
          ) : null}
        </div>
      )}
    </div>
  );
}

interface RoundBodyProps {
  entry: ChatBlockEntry | undefined;
  chatSessionId: string;
  reload: (chatId: string, refs: string[]) => void;
  /** The ref this round is read under — the retry asks for exactly it. */
  ref_: string;
  status: string;
  failed: boolean;
}

/** The answer panel wears the agent bubble's fill (`bg-chat-bubble`), the same
 *  surface the same words get in the list; a red wash when it is an outage or a
 *  failure. The highlight belongs to the human's line above it — in the
 *  transcript a reply never wears it. Anything that is NOT an answer states what
 *  is missing instead of painting a blank box. */
function RoundBody({
  entry,
  chatSessionId,
  reload,
  ref_,
  status,
  failed,
}: RoundBodyProps) {
  const { t } = useTranslation();
  const body = entry?.projection?.answer?.body.trim() ?? "";

  if (!entry) {
    return (
      <div
        className="rounded bg-chat-bubble p-2 text-text-tertiary"
        style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
      >
        {t("chatView.block.loading")}
      </div>
    );
  }
  if (entry.status === "unreachable") {
    return (
      <div
        className="flex flex-wrap items-center gap-2 rounded bg-red-50 p-2 text-red-600 dark:bg-red-900/20 dark:text-red-400"
        style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
      >
        <span>{t("chatView.block.unreachable")}</span>
        <button
          type="button"
          className="underline underline-offset-2"
          onClick={() => reload(chatSessionId, [ref_])}
        >
          {t("chatView.block.retry")}
        </button>
      </div>
    );
  }
  if (entry.status === "not_found" || !entry.projection) {
    return (
      <div
        className="rounded bg-chat-bubble p-2 text-text-tertiary"
        style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
      >
        {t("chatView.block.notFound")}
      </div>
    );
  }
  if (body) {
    return (
      <div
        className={`rounded p-2 ${failed ? "bg-red-50 text-red-600 dark:bg-red-900/20 dark:text-red-400" : "bg-chat-bubble text-text-secondary"}`}
        style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
      >
        <ChatMarkdown content={body} />
      </div>
    );
  }
  // No answer yet. A run in flight says so in the status copy the composer's own
  // turn line uses; a settled round with nothing to show reports the outcome,
  // because how it ended is a fact the transcript owes the reader.
  return (
    <div
      className="rounded bg-zinc-100 p-2 text-text-tertiary dark:bg-zinc-800"
      style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
    >
      {status ? t(statusKey(status)) : t("chatView.block.unknown")}
    </div>
  );
}

interface BlockCardActionsProps {
  card: BlockCard;
  canAnswer: boolean;
  instanceId: string;
  execSid: string;
  /** The reference row's own id — the coordinate a continuation addresses (D6
   *  level 1). */
  rowId: string;
  onReplyToRound?: (rowId: string, quote: string) => void;
}

/** The round's pending decision, answered against the EXECUTION session (D8).
 *  `request_id` is an unguessable capability the projection only hands to a
 *  participant, which is why no chat-side copy is needed to close it. The
 *  buttons are `ExploreBlock`'s approval pair — the same decision, on the same
 *  surface family, in the same chrome. */
function BlockCardActions({
  card,
  canAnswer,
  instanceId,
  execSid,
  rowId,
  onReplyToRound,
}: BlockCardActionsProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  const [freeText, setFreeText] = useState("");

  const kind = card.kind ?? "ask_question";
  const Icon =
    kind === "approval" ? ShieldCheck : kind === "pause" ? Pause : MessageCircleQuestion;
  const titleKey =
    kind === "approval"
      ? "chatView.cardApproval"
      : kind === "pause"
        ? "chatView.cardPause"
        : "chatView.cardAsk";

  const post = async (run: () => Promise<void>) => {
    setBusy(true);
    setFailed(false);
    try {
      await run();
      // The execution surface keeps its own copy of the pending request, and
      // the MQTT clear signal (a zero-byte retained publish) is unreliable for
      // already-connected subscribers under rumqttd — so dismiss it locally too
      // rather than leaving a live card next to an answered block.
      useChatStore
        .getState()
        .resolveQuestion(instanceId, card.request_id, execSid);
    } catch {
      // The card stays open: the round is still parked on the question, and a
      // silent failure would strand the human with a button that did nothing.
      setFailed(true);
    } finally {
      setBusy(false);
    }
  };

  const denyBtn =
    "rounded-md border border-zinc-300 px-2 py-0.5 text-11 font-medium text-text-secondary transition-colors hover:bg-zinc-200 disabled:opacity-50 dark:border-zinc-500  dark:hover:bg-zinc-600";
  const allowBtn =
    "rounded-md px-2 py-0.5 text-11 font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-50";

  // A pause has no execution-side request to resolve: the turn already ended
  // `waiting`, and the human's reply IS the next turn of this round (D6
  // level 1) — so it rides the composer, addressed to the reference row.
  if (kind === "pause") {
    return (
      <div className="space-y-1 py-1">
        <div className="flex items-center gap-1.5 text-text-secondary">
          <Icon className="h-3.5 w-3.5 shrink-0" />
          <span>{t(titleKey)}</span>
        </div>
        {card.prompt ? (
          <div className="text-text-secondary">
            <ChatMarkdown content={card.prompt} />
          </div>
        ) : null}
        {canAnswer && onReplyToRound ? (
          <button
            type="button"
            className={allowBtn}
            style={{ backgroundColor: "var(--color-accent-strong)" }}
            onClick={() => onReplyToRound(rowId, card.prompt ?? "")}
          >
            {t("chatView.cardContinue")}
          </button>
        ) : null}
      </div>
    );
  }

  const options = card.options ?? [];
  return (
    <div className="space-y-1 py-1">
      <div className="flex items-center gap-1.5 text-text-secondary">
        <Icon className="h-3.5 w-3.5 shrink-0" />
        <span>{t(titleKey)}</span>
      </div>
      {card.prompt ? (
        <div className="text-text-secondary">
          <ChatMarkdown content={card.prompt} />
        </div>
      ) : null}
      {!canAnswer ? (
        <p className="text-text-tertiary">{t("chatView.block.answeredByOwner")}</p>
      ) : (
        <div className="flex flex-wrap items-center gap-1.5">
          {kind === "approval" ? (
            <>
              <button
                type="button"
                disabled={busy}
                className={denyBtn}
                onClick={() =>
                  post(() =>
                    sendApproval(instanceId, execSid, {
                      request_id: card.request_id,
                      approved: false,
                    }),
                  )
                }
              >
                {t("chatView.cardReject")}
              </button>
              <button
                type="button"
                disabled={busy}
                className={allowBtn}
                style={{ backgroundColor: "var(--color-accent-strong)" }}
                onClick={() =>
                  post(() =>
                    sendApproval(instanceId, execSid, {
                      request_id: card.request_id,
                      approved: true,
                    }),
                  )
                }
              >
                {t("chatView.cardApprove")}
              </button>
            </>
          ) : (
            options.map((opt, i) => (
              <button
                key={opt}
                type="button"
                disabled={busy}
                className={i === 0 ? allowBtn : denyBtn}
                style={
                  i === 0
                    ? { backgroundColor: "var(--color-accent-strong)" }
                    : undefined
                }
                onClick={() =>
                  post(() =>
                    sendAnswer(instanceId, execSid, {
                      request_id: card.request_id,
                      answer: opt,
                    }),
                  )
                }
              >
                {opt}
              </button>
            ))
          )}
          {kind === "ask_question" ? (
            <span className="flex items-center gap-1">
              <input
                value={freeText}
                onChange={(e) => setFreeText(e.target.value)}
                placeholder={t("chatView.block.otherAnswer")}
                className="rounded border border-zinc-300 bg-transparent px-1.5 py-0.5 dark:border-zinc-500"
                style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
              />
              <button
                type="button"
                disabled={busy || !freeText.trim()}
                className={denyBtn}
                onClick={() =>
                  post(() =>
                    sendAnswer(instanceId, execSid, {
                      request_id: card.request_id,
                      answer: freeText.trim(),
                    }),
                  ).then(() => setFreeText(""))
                }
              >
                {t("chatView.answer")}
              </button>
            </span>
          ) : null}
          {failed ? (
            <span className="text-red-500">{t("chatView.block.answerFailed")}</span>
          ) : null}
        </div>
      )}
    </div>
  );
}

interface BlockInlineReplyProps {
  chatSessionId: string;
  /** The freshest round's reference row id — replying to it continues THAT run
   *  (D6 level 1); the backend resolves the row into its session. Empty means
   *  the block has no round to answer yet, so the box does nothing. */
  rowId: string;
  /** A rising edge (the `@` picker's "去续投") reveals and focuses the box. */
  focusSignal?: boolean;
}

/**
 * The block's in-place reply box (ADR-106 revision 2026-11-04).
 *
 * Continuing a run used to mean borrowing the main composer, which made the
 * human leave the block they were pointing at. This sends `sendGroupText(chat,
 * text, [], rowId)` — the existing level-1 branch — so the claim and the wait
 * line fall out of the same machinery a card answer already uses, and a reply
 * to a specific round never re-opens the routing question.
 *
 * The IM habit it has to respect: the box reveals on hover or focus, but a
 * non-empty draft or held focus NEVER collapses it (losing typed words to a
 * stray mouse-out is the failure), and it only hides when it is empty AND
 * unfocused AND the pointer has left. After a send it clears and stays put, so
 * a second follow-up is one keystroke away.
 */
function BlockInlineReply({
  chatSessionId,
  rowId,
  focusSignal,
}: BlockInlineReplyProps) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState("");
  const [hovering, setHovering] = useState(false);
  const [focused, setFocused] = useState(false);
  const [sending, setSending] = useState(false);
  const boxRef = useRef<HTMLTextAreaElement>(null);

  // A locate from the picker reveals + focuses the box on its rising edge, so
  // the human arrives ready to type rather than hunting for a hidden control.
  const lastSignal = useRef(false);
  useEffect(() => {
    if (focusSignal && !lastSignal.current) {
      boxRef.current?.focus();
    }
    lastSignal.current = focusSignal ?? false;
  }, [focusSignal]);

  const visible = hovering || focused || draft.length > 0;

  const send = async () => {
    const text = draft.trim();
    if (!text || !rowId || sending) return;
    setSending(true);
    try {
      await useChatSessionStore
        .getState()
        .sendGroupText(chatSessionId, text, [], rowId);
      // Clear and keep the box: an in-place continuation invites the next one.
      setDraft("");
    } finally {
      setSending(false);
    }
  };

  return (
    <div
      className="mt-1"
      onMouseEnter={() => setHovering(true)}
      onMouseLeave={() => setHovering(false)}
    >
      <div
        className={cn(
          "flex items-end gap-1 rounded-md border border-border-outer bg-surface px-1.5 py-1 transition-opacity",
          visible ? "opacity-100" : "pointer-events-none opacity-0",
        )}
      >
        <textarea
          ref={boxRef}
          rows={1}
          value={draft}
          disabled={sending}
          aria-label={t("chatView.block.replyPlaceholder")}
          placeholder={t("chatView.block.replyPlaceholder")}
          onFocus={() => setFocused(true)}
          onBlur={() => setFocused(false)}
          onChange={(e) => setDraft(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter" && !e.shiftKey) {
              e.preventDefault();
              void send();
            }
          }}
          className="min-h-[1.5rem] w-full resize-none border-0 bg-transparent px-1 outline-none placeholder:text-text-disabled disabled:opacity-50"
          style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
        />
        <button
          type="button"
          onClick={() => void send()}
          disabled={!draft.trim() || sending || !rowId}
          aria-label={t("chatView.block.send")}
          className="inline-flex h-6 w-6 shrink-0 items-center justify-center rounded text-brand-primary hover:bg-[var(--color-accent)]/10 disabled:opacity-40"
        >
          <Send className="h-3.5 w-3.5" />
        </button>
      </div>
    </div>
  );
}
