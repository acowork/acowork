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
 *
 * What the reader sees without clicking, and why:
 * - the block AND its rounds start FOLDED. Reading the work is a deliberate
 *   gesture; watching a session iterate is not what a Chat is for. The one
 *   thing that opens the block on its own is an unanswered card (D8): an
 *   answerable control behind a fold is not answerable, so the block and the
 *   parked round open together.
 * - the run's latest answer is drawn BELOW the block as an ordinary assistant
 *   bubble (`data-run-answer`). That is the Chat's own grammar — the agent's
 *   newest words sit in the transcript — while the rounds stay one click away.
 *   The text is the same projected `answer` the collapsed round shows: no
 *   second read, no new row, no invented content. The bubble itself is folded
 *   to ten lines (`RunAnswerBubble`): an agent's answer is usually long, and an
 *   unclamped bubble under every block turns the transcript into a wall of
 *   text — the conversation's shape is what the reader is paging through.
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
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import type { BlockCard, ChatBlockEntry, RoundProjection } from "../../lib/chatBlocks";
import type { RunBlock } from "./chatTranscriptGroups";
import { useChatBlockStore, useChatBlocks } from "../../stores/chatBlockStore";
import { useChatStore } from "../../stores/chatStore";
import { useChatSessionStore } from "../../stores/chatSessionStore";
import { useAgentStore } from "../../stores/agentStore";
import { useSessionTitle } from "../../lib/useSessionTitle";
import { SpeakerBadge } from "./speakerNames";
import { sendAnswer, sendApproval } from "../../lib/session-control";
import { deriveSessionTitle } from "../../lib/sessionConfigMapper";
import { getProcessingPhase, isProcessing, type SessionStatus } from "../../lib/types";
import { AgentAvatar } from "../common/AgentAvatar";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { RunBlockActionRow } from "./RunBlockActionRow";
import { ChatMarkdown } from "./ChatMarkdown";
import { RunAnswerBubble } from "./RunAnswerBubble";
import { IntentDisplayBody } from "./IntentDisplayBody";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import { InteractionCardShell } from "./InteractionCardShell";
import { QuestionAnswerForm } from "./QuestionAnswerForm";
import { cardButtonAccent, cardButtonNeutral } from "../../lib/ui-styles";
import type { CardAnswerHandler, CardChoice } from "../../lib/chatCardAnswer";
import { BLOCK_DETAIL_FONT_SIZE, BLOCK_FONT_SIZE } from "./blockTypeScale";
import { runBlockToText } from "../../lib/runBlockText";

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
  /** Drill into the execution session that owns these rounds. */
  onJumpToExec?: (agentInstanceId: string, execSid: string) => void;
  /** Address the composer to one round (D6 level 1) — what a `pause` card's
   *  "continue" and any follow-up on a finished round ride. */
  onReplyToRound?: (rowId: string, quote: string) => void;
  /** A pause card's Continue, sent as a plain "Continue" message straight to
   *  the execution session's own write endpoint — the same request the
   *  execution composer makes when a human types it. It is a control signal
   *  for that session's loop, NOT a chat message: the chat send path never
   *  sees it, so no user bubble lands in the transcript and the Chat Router
   *  cannot hijack it into a new session. Absent, the button falls back to
   *  addressing the composer. */
  onContinueRound?: (instanceId: string, execSessionId: string) => void;
  /** Attach this execution session to the composer as a reference (ADR-106 D7):
   *  the block's `@` action. The host owns the composer's address state, so the
   *  block reports the intent and never touches it directly. Absent on a surface
   *  that cannot carry a reference (a read-only chat). */
  onReferenceRun?: (payload: {
    instanceId: string;
    execSessionId: string;
    name: string;
  }) => void;
  /** A viewer who is not a round's answerer (`answerable_by`): the question
   *  shows, the buttons do not (D10's read/write split). */
  canAnswer?: boolean;
  /** Answer the router's addressing question about THIS run (ADR-106 D6 level
   *  5). The card is drawn at the block's tail — the layout hangs it here
   *  (`RunBlock.pendingAsk`) so the question sits where its subject is.
   *  Unless the host docked it (`pendingAskDocked`), which is the placement
   *  this surface uses: the question floats at the bottom of the surface and
   *  the run wears a badge instead. */
  onChooseCard?: CardAnswerHandler;
  /** The host docked this run's question at the bottom of the surface (ADR-106
   *  revision 2026-11-11): it is drawn THERE, so the tail must not draw it a
   *  second time. The run still wears the badge that names it as the
   *  question's subject. */
  pendingAskDocked?: boolean;
  /** Group surface only: right-click the author → the panel's "@name" menu. */
  onAuthorContextMenu?: (e: React.MouseEvent) => void;
  /** ADR-106 revision 2026-11-04: the `@` picker's "去续投" located this block
   *  and wants its in-place input focused. A rising edge opens the action row's
   *  reply drawer and puts the caret in it, so the human lands ready to type
   *  instead of hunting. */
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

/**
 * The question ONE round leads with. Two consumers ask this — the round row's
 * summary/body and the block's copy export — and they must answer it
 * identically: a copied run that quotes a different question than the one on
 * screen is a document the reader cannot point at.
 *
 * The order is the transcript's own: a round that shows its own user line leads
 * with what the human wrote there; a round whose sentence already stands on the
 * transcript as its own bubble (the opener) leads with the reference row's
 * one-line `round_name` — NOT with the projection's stored question, which
 * is the *delivered prompt* and carries ADR-107's reference parts
 * (`[user message]` + the raw sentence) verbatim.
 *
 * `round_name` is therefore the opener's line by contract, and the Runtime
 * owns what may be written there: `reference_name` refuses the PM wake-up
 * playbook (a system slot, design 27 §5.4.2) and names a dispatch by its own
 * text. The header's name chain does not read the brief at all, so a row written
 * before that guard cannot put the playbook on the block's title.
 */
function questionOf(
  round: RunBlock["rounds"][number],
  projection: RoundProjection | undefined,
): string {
  return (
    (round.showUserLine ? (round.triggerRow?.body ?? "") : "") ||
    (round.questionOnTranscript
      ? (round.roundRow.meta?.round_name ??
          // Rows written before the rename carry the same fact under the key it
          // borrowed from the envelope's PM playbook. Read, never written.
          round.roundRow.meta?.assignment_brief ??
          "")
      : "") ||
    // An agent-origin round (design 27 §5.4.5): the row body is the machine
    // envelope, and the display projection beside it is the sentence a human
    // reads. `summary` first (a title when the sender gave one) and the body
    // after it, because a round row's line is a name, not a paragraph.
    projection?.user?.display?.summary ||
    projection?.user?.display?.body ||
    projection?.user?.body ||
    ""
  );
}

/**
 * The question a reader EXPANDS into — the full text, not the row's one-liner.
 *
 * Same precedence as {@link questionOf} with the display's `body` ahead of its
 * `summary`: the collapsed row names the round, the open round shows it.
 */
function questionBodyOf(
  round: RunBlock["rounds"][number],
  projection: RoundProjection | undefined,
): string {
  // A display PRESENT is a decision, not a hint: the turn has a human rendering,
  // and its empty `body` means the producer wrote only a one-line `summary`.
  // Falling through to `user.body` there painted the machine envelope into the
  // expanded panel — the very text the display exists to replace.
  const display = projection?.user?.display;
  if (display) return display.body || display.summary;
  return (
    (round.showUserLine ? (round.triggerRow?.body ?? "") : "") ||
    projection?.user?.body ||
    ""
  );
}

/**
 * What a round row's status glyph says, derived once. A `pending` read is the
 * only positive evidence a round is still going (D9), and a round parked on an
 * unanswered card is waiting for the human instead (D8) — the two rules the row
 * and the copy export must not state differently.
 */
function roundStatus(entry: ChatBlockEntry | undefined): string {
  const projection = entry?.projection;
  if (!entry) return "unknown";
  if (entry.status === "pending") {
    // Parked on an open card = waiting for the human, not executing.
    return projection?.card ? "waiting" : "in_progress";
  }
  return entry.status === "ok" ? (projection?.status ?? "unknown") : "unknown";
}

export const ChatExecutionBlock = memo(function ChatExecutionBlock({
  block,
  chatSessionId,
  showHeader,
  authorName,
  onJumpToExec,
  onReplyToRound,
  onContinueRound,
  onReferenceRun,
  canAnswer = true,
  onAuthorContextMenu,
  focusInput = false,
  onChooseCard,
  pendingAskDocked = false,
  className,
}: ChatExecutionBlockProps) {
  const { t } = useTranslation();
  const instanceId = block.instanceId;
  const execSid = block.execSessionId;
  const refs = useMemo(() => block.rounds.map((r) => r.ref), [block]);
  /** The addressing question hung on this run (layout's `pendingAsk`), or none. */
  const pendingAsk = block.pendingAsk;
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
  /** The freshest round's reference-row id — the coordinate an in-place
   *  continuation addresses (D6 level 1). Empty means the block has no round to
   *  answer yet, so the drawer's send stays inert. */
  const latestRowId = newest?.roundRow.meta?.message_id ?? "";
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
  // spinner stays tied to this block's own pending read (D9).
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

  // Folded by default, and it stays folded until the human opens it. A run that
  // is still working does not unfold itself either: the freshest answer is
  // drawn below the block as a bubble, so watching the session iterate is not a
  // precondition for reading it. The ONE condition that opens the block on its
  // own is an unanswered card — an answerable control behind a fold is not
  // answerable (D8) — and it is not a "default", it is the human being asked.
  const [expanded, setExpanded] = useState(false);
  const contentRef = useRef<HTMLDivElement | null>(null);

  useEffect(() => {
    if (hasOpenCard) {
      // The answer buttons have to be on screen, not just in the pane.
      setExpanded(true);
      setTimeout(() => {
        if (contentRef.current) {
          contentRef.current.scrollTop = contentRef.current.scrollHeight;
        }
      }, 0);
    }
  }, [hasOpenCard]);

  const name =
    runName?.trim() ||
    // The session's own opening question — the same text the optimistic title
    // seed is cut from, so a block and the tab that later shows its session do
    // not name the same run two different ways.
    deriveSessionTitle(
      block.rounds.find((r) => r.triggerRow)?.triggerRow?.body ?? "",
    ) ||
    // No Chat row opened this run: a PM dispatch (ADR-106 D12 writes no row —
    // the dispatch IS the round's first user row) and a turn composed on the
    // execution surface both land here. The question the session actually
    // received is the only human text the run has, so it names the run; without
    // it a task block reads "unnamed run" beside its own task chip, which is
    // how the dispatch appeared to vanish from the group.
    // A peer round's `user.body` is the machine envelope, so the run is named by
    // the display projection when the turn has one (design 27 §5.4.5) — the same
    // sentence the exec session's own title is seeded from.
    deriveSessionTitle(
      block.rounds
        .map((round) => {
          const user = entries[round.ref]?.projection?.user;
          return (
            user?.display?.summary || user?.display?.body || user?.body || ""
          );
        })
        .find((body) => body.trim() !== "") ?? "",
    ) ||
    t("chatView.block.unnamedRun");
  // The author's one name: their own `name`, or the raw id when the row
  // carries no metadata at all. No second, "display" candidate to fall back
  // to — a fallback chain is how two names for one participant appear.
  const displayName = authorName?.trim() || meta?.name || instanceId;
  // The task coordinate (ADR-106 D13): it rides the ROUND, not the utterance, and
  // the header is where a reader looks for "which piece of work is this". A
  // session bound to a task carries the same id on every round, so the first one
  // wins; a round that carries none paints nothing.
  const taskId = block.rounds.find((r) => r.roundRow.meta?.task_id)?.roundRow
    .meta?.task_id;

  // The "copy" payload (design 27 §5.5.5): the WHOLE session, folded rounds
  // included — the button copies the run, and a run that dropped its first five
  // rounds would read as if the session started at round six. A round whose
  // content the transcript cannot read exports as a stated hole rather than an
  // invented answer (D3/D5).
  const copyPayload = useMemo(
    () =>
      runBlockToText({
        name,
        labels: {
          round: (n: number) => t("chatView.block.roundN", { n }),
          questionPrefix: t("chatView.block.copyQuestionPrefix"),
          unreadable: t("chatView.block.copyUnreadable"),
        },
        rounds: block.rounds.map((round, i) => {
          const entry = entries[round.ref];
          const projection = entry?.projection;
          const answer = projection?.answer?.body.trim() ?? "";
          const status = roundStatus(entry);
          return {
            index: i + 1,
            // The export is a document a reader points at, so it carries the
            // full question the round shows when expanded — for an agent-origin
            // round that is the display body, not the `[intent:…] {json}` row.
            question: questionBodyOf(round, projection),
            // No answer body: the round's outcome is what it has to say, in the
            // same words its row uses.
            answer: answer || (projection ? t(statusKey(status)) : ""),
            unreadable: !projection,
          };
        }),
      }),
    [block, entries, name, t],
  );

  // The run's last word, drawn OUTSIDE the fold (the bubble at the tail of this
  // component). It is read from the very projection the collapsed rounds carry,
  // so nothing new is fetched and no row is invented; a round still working has
  // not spoken yet, so the newest round WITH an answer is the run's latest
  // utterance. It re-renders whenever the block read lands a newer answer — the
  // block store's re-read of a `pending` ref is what makes that live.
  const latestAnswer = useMemo(() => {
    for (let i = block.rounds.length - 1; i >= 0; i -= 1) {
      const body = entries[block.rounds[i]!.ref]?.projection?.answer?.body.trim();
      if (body) return body;
    }
    return "";
  }, [block, entries]);

  return (
    // The run the router is asking about wears a light accent wash and a
    // badge. The question itself floats at the bottom of the surface (ADR-106
    // revision 2026-11-11), so this is what keeps the question and its subject
    // together — the same accent wash the locate highlight uses.
    <div
      className={`px-4 py-1 ${pendingAsk ? "bg-[var(--color-accent)]/10" : ""} ${className ?? ""}`}
    >
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
            {/* The block's affordances, one row above it (design 27 §5.5.5):
                open the run, reference it, copy it, continue it. The row owns
                its own reply drawer, so nothing below the block reserves space
                any more. */}
            <RunBlockActionRow
              runName={runName ?? name}
              agentId={instanceId}
              sessionId={execSid}
              chatSessionId={chatSessionId}
              canManage={canAnswer !== false}
              onJump={
                onJumpToExec
                  ? () => onJumpToExec(instanceId, execSid)
                  : undefined
              }
              onReference={
                onReferenceRun && canAnswer
                  ? () =>
                      onReferenceRun({
                        instanceId,
                        execSessionId: execSid,
                        name,
                      })
                  : undefined
              }
              copyPayload={copyPayload}
              onSend={
                canAnswer
                  ? async (text, attachmentIds) => {
                      // The level-1 branch (D6): the freshest round's reference
                      // row, the same coordinate the pause card's continue and
                      // the `@` picker's "去续投" address. The claim and the wait
                      // line fall out of the machinery a card answer uses.
                      // `attachmentIds` are the files the row's `+` menu picked
                      // (uploaded into this Chat session); the router forwards
                      // them to the run as `chat_attachment_ids`.
                      await useChatSessionStore
                        .getState()
                        .sendGroupText(
                          chatSessionId,
                          text,
                          [],
                          latestRowId,
                          attachmentIds.length > 0 ? attachmentIds : undefined,
                        );
                    }
                  : undefined
              }
              canSend={!!latestRowId}
              focusSignal={focusInput}
            />
            <div className="max-w-full" style={{ fontSize: BLOCK_FONT_SIZE }}>
              <button
                onClick={() => setExpanded((v) => !v)}
                aria-expanded={expanded}
                // Stable hook for "the block's own collapse header": the action
                // row above now carries `aria-expanded` too (its drawer), so a
                // query on that generic attribute finds two different things.
                data-run-header=""
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
                {/* The question about THIS run is docked at the bottom of the
                    surface, so the run says so: a reader who sees the badge
                    knows where the question went, and which run it is about. */}
                {pendingAsk && (
                  <span
                    data-testid="exec-block-ask-badge"
                    className="shrink-0 rounded bg-[var(--color-accent)]/15 px-1.5 py-0.5 text-[0.8em] text-text-secondary"
                  >
                    {t("chatView.block.unanswered")}
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
                        chatSessionId={chatSessionId}
                        instanceId={instanceId}
                        execSid={execSid}
                        entry={entries[round.ref]}
                        reload={reload}
                        canAnswer={canAnswer}
                        onReplyToRound={onReplyToRound}
                        onContinueRound={onContinueRound}
                      />
                    ))}
                  </div>
                </OverlayScrollbar>
              )}
            </div>
            {/* The run's latest answer, in the transcript's own assistant-bubble
                skin — under the block, never inside the fold. This is what keeps
                the surface reading as a conversation: the agent's newest words
                are on screen while the rounds stay one click away.
                It is the SAME projected `answer` the newest answered round
                carries (`latestAnswer`), so nothing here can say anything the
                block would not; a run that has not answered yet paints nothing
                instead of an empty bubble. */}
            {latestAnswer && <RunAnswerBubble content={latestAnswer} />}
          </div>
          {/* The router's question about this run, drawn at the run's tail and
              outside the collapse: an unanswerable thing behind a fold is the
              one shape D8 forbids, and the human has to be able to answer it
              from the block it belongs to.
              `identity="inherited"`: the block's own header, right above, already
              wears this agent's face and name at the 40px gutter, so the card
              adds neither and starts where the rounds start. A second avatar
              over one question is what made this card look misplaced.

              The host usually draws it elsewhere — docked at the bottom of the
              surface (ADR-106 revision 2026-11-11), where the human sees it
              without hunting for the run. Then the tail stays empty: one
              question, one place. */}
          {pendingAsk && !pendingAskDocked && (
            <div className="mt-1">
              <ChatNoticeEventRow
                row={pendingAsk}
                identity="inherited"
                onChoose={
                  canAnswer && onChooseCard
                    ? (choice: CardChoice) => onChooseCard(pendingAsk, choice)
                    : undefined
                }
              />
            </div>
          )}
        </div>
      </div>
    </div>
  );
});

interface RoundItemProps {
  round: RunBlock["rounds"][number];
  /** Position in the session's round list (1-based) — the row's label. */
  index: number;
  chatSessionId: string;
  instanceId: string;
  execSid: string;
  entry: ChatBlockEntry | undefined;
  reload: (chatId: string, refs: string[]) => void;
  canAnswer: boolean;
  onReplyToRound?: (rowId: string, quote: string) => void;
  onContinueRound?: (instanceId: string, execSessionId: string) => void;
}

/** One round: a question line that opens into the question/answer pair, in
 *  `ExploreBlock`'s row-plus-two-panels chrome. Folded like the block itself —
 *  the run's answer is already on screen below it (`data-run-answer`), and the
 *  pair is what a reader opens when they want the question beside it. */
function RoundItem({
  round,
  index,
  chatSessionId,
  instanceId,
  execSid,
  entry,
  reload,
  canAnswer,
  onReplyToRound,
  onContinueRound,
}: RoundItemProps) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const projection = entry?.projection;
  // One answer for the row's line and the copy export (`questionOf`), one for
  // the expanded panel (`questionBodyOf`): an opener round's sentence already
  // stands on the transcript as its own bubble, so the row leads with the
  // reference row's one-line name and does not repeat the raw text; an
  // agent-origin round has no bubble at all, and shows the display projection it
  // carries instead of the machine envelope (design 27 §5.4.5).
  const question = questionOf(round, projection);
  const questionBody = questionBodyOf(round, projection);
  // The machine format, kept reachable but never painted by default: this is the
  // envelope the model reads (coordinates, `params`).
  const rawEnvelope = projection?.user?.display ? (projection.user.body ?? "") : "";
  const display = projection?.user?.display ?? null;
  // Plan D3: a round whose row was written by a human standing in for the run's
  // initiator says so here too. The Chat's block is the second surface that
  // shows these words, and a disclosure on the bubble but not here would let the
  // same sentence read as the agent's on one side of the app and as a person's on
  // the other. `SpeakerBadge` says NAMES, not the ids the record carries
  // (resolved in `speakerNames.ts`), and renders nothing when there is no
  // disclosure to make.
  // The words of a turn composed in the execution view: it has no Chat row to
  // pair with, so `showUserLine` is true with nothing to show
  // (`triggerRow` absent) and the block used to fall through to "no question".
  // Now that a human can keep an agent-to-agent run alive from that view, that
  // gap is the normal case rather than an edge: the round's own user body IS the
  // sentence, and it is the human's literal text, so it needs no display layer.
  const userBody = projection?.user?.body ?? "";
  const showComposedLine = !round.showUserLine && !display && userBody.trim().length > 0;
  // A round with nothing readable says so instead of borrowing the wrong line.
  const summary = summarize(question) || t("chatView.block.noQuestion");
  const pending = entry?.status === "pending";
  // Only a round that can still take an answer wears a card: an open one, or a
  // `waiting` park the human has not replied to yet. The server strips the rest
  // (`chat_blocks::sanitize_card`); this is the same rule where the pixels are,
  // because a stale entry may have been read from an older server or cache.
  const card =
    projection?.card && (pending || projection.status === "waiting")
      ? projection.card
      : null;
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
        <button
          onClick={() => setOpen((v) => !v)}
          aria-label={t("chatView.block.toggleRound")}
          aria-expanded={open}
        >
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
          {/* An agent-origin round has no bubble of its own on the transcript
              (ADR-106 D12 writes no chat row for the exchange), so this is the
              only place its question appears — as the display projection the
              turn carried, in Markdown, with the machine envelope one click
              away. Painting `projection.user.body` here is what read
              `[intent:2f3c…:analyze] {"title":…}` to a human. */}
          {!round.showUserLine && display && questionBody && (
            <div
              className="rounded bg-[var(--color-accent)]/10 p-2 text-text-secondary"
              style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
            >
              <SpeakerBadge
                speaker={projection?.user?.speaker}
                className="mb-1 block text-text-tertiary"
              />
              <IntentDisplayBody display={display} envelope={rawEnvelope} />
            </div>
          )}
          {/* A round whose words came from the execution view: no Chat row to
              pair with, so there is no bubble above and no display projection
              (the author is a person, whose text is already literal). Rendering
              nothing left the block printing "no question" over a round that has
              one — the exact gap the intervention tier turns into the common
              case. */}
          {showComposedLine && (
            <div
              className="rounded bg-[var(--color-accent)]/10 p-2 text-text-secondary"
              style={{ fontSize: BLOCK_DETAIL_FONT_SIZE }}
            >
              <SpeakerBadge
                speaker={projection?.user?.speaker}
                className="mb-1 block text-text-tertiary"
              />
              <pre className="whitespace-pre-wrap break-words">{userBody}</pre>
            </div>
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
              reloadRound={() => reload(chatSessionId, [round.ref])}
              rowId={round.roundRow.meta?.message_id ?? ""}
              onReplyToRound={onReplyToRound}
              onContinueRound={onContinueRound}
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
  /** Re-read this round after an answer, whatever the answer's status says. */
  reloadRound: () => void;
  /** The reference row's own id — the coordinate a continuation addresses (D6
   *  level 1). */
  rowId: string;
  onReplyToRound?: (rowId: string, quote: string) => void;
  onContinueRound?: (instanceId: string, execSessionId: string) => void;
}
/**
 * The round's pending decision, answered against the EXECUTION session (D8).
 *  `request_id` is an unguessable capability the projection only hands to a
 *  participant, which is why no chat-side copy is needed to close it.
 *
 *  The chrome is the shared `InteractionCardShell` + `QuestionAnswerForm` —
 *  the same components `AskQuestionCard` wears on the execution surface, so
 *  one question does not change costume between surfaces. (This function used
 *  to re-invent that: a bare title row, full-width option buttons whose first
 *  was accent-washed, and an unstyled `<input>` next to a small button —
 *  labels that did not read as labels, buttons that did not read as buttons.)
 */
function BlockCardActions({
  card,
  canAnswer,
  instanceId,
  execSid,
  reloadRound,
  rowId,
  onReplyToRound,
  onContinueRound,
}: BlockCardActionsProps) {
  const { t } = useTranslation();
  const [busy, setBusy] = useState(false);
  const [failed, setFailed] = useState(false);
  // A Continue is a state transition, not a message: once sent, this pause is
  // over as far as this surface is concerned — a second click would ask the
  // execution session to resume a pause it has already left. Keyed by
  // `request_id`, so a genuinely new pause on the same row gets a live button.
  const [sentFor, setSentFor] = useState<string | undefined>(undefined);

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
      // Re-read the round in both outcomes. `POST /answer` queues the message
      // and answers `202` — it does not, and cannot, tell a live request from
      // one whose timeout already fired (the turn then drops the answer). So
      // the click's own status is not evidence about the card: the projection
      // is. Dropping a card only locally meant a dead question stayed on screen
      // looking clickable until a newer row pushed it out of view.
      reloadRound();
    }
  };

  const failedNote = failed ? (
    <span className="text-red-500">{t("chatView.block.answerFailed")}</span>
  ) : undefined;

  // A viewer who is not a round's answerer (D10): the question shows, the
  // controls do not — the click is the speaker's, and a dead button would
  // promise a decision this surface cannot make.
  const notYours = (
    <p className="text-text-tertiary">{t("chatView.block.answeredByOwner")}</p>
  );

  // A pause has no execution-side request to resolve: the turn already ended
  // `waiting`, and the human's reply IS the next turn of this round (D6
  // level 1) — so it rides the composer, addressed to the reference row.
  if (kind === "pause") {
    return (
      <InteractionCardShell icon={Icon} title={t(titleKey)}>
        {card.prompt ? (
          <div className="mb-2">
            <ChatMarkdown content={card.prompt} />
          </div>
        ) : null}
        {canAnswer && (onContinueRound || onReplyToRound) ? (
          <button
            type="button"
            className={cardButtonAccent}
            style={{ backgroundColor: "var(--color-accent-strong)" }}
            // Continue is a control signal for the execution session's loop,
            // not a chat message: it goes to the execution session's own
            // write endpoint — the same request the execution composer makes
            // when a human types "Continue". The chat send path never sees
            // it, so no user bubble lands in the transcript and the Chat
            // Router cannot hijack it into a new session. Without an
            // execution id on the projection, fall back to the composer.
            disabled={sentFor === card.request_id}
            onClick={() => {
              setSentFor(card.request_id);
              if (execSid && onContinueRound) onContinueRound(instanceId, execSid);
              else onReplyToRound?.(rowId, card.prompt ?? "");
            }}
          >
            {t("chatView.cardContinue")}
          </button>
        ) : (
          notYours
        )}
      </InteractionCardShell>
    );
  }

  if (kind === "approval") {
    return (
      <InteractionCardShell icon={Icon} title={t(titleKey)}>
        {card.prompt ? <ChatMarkdown content={card.prompt} /> : null}
        {canAnswer ? (
          <div className="mt-2 flex items-center gap-1.5">
            <button
              type="button"
              disabled={busy}
              className={cardButtonNeutral}
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
              className={cardButtonAccent}
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
            {failedNote}
          </div>
        ) : (
          <div className="mt-2">{notYours}</div>
        )}
      </InteractionCardShell>
    );
  }

  // `ask_question`: the execution card's own body — radio list, "Other"
  // textarea, one submit. The options of an `ask_question` are sentences a
  // model wrote, not labels, and the form wraps them instead of breaking
  // them mid-phrase in a button row.
  return (
    <InteractionCardShell icon={Icon} title={t(titleKey)}>
      {card.prompt ? <ChatMarkdown content={card.prompt} /> : null}
      {canAnswer ? (
        <div className="mt-2">
          <QuestionAnswerForm
            name={card.request_id}
            options={(card.options ?? []).map((label) => ({ label }))}
            disabled={busy}
            note={failedNote}
            onSubmit={(answer) =>
              post(() =>
                sendAnswer(instanceId, execSid, {
                  request_id: card.request_id,
                  answer,
                }),
              )
            }
          />
        </div>
      ) : (
        <div className="mt-2">{notYours}</div>
      )}
    </InteractionCardShell>
  );
}
