/**
 * ChatExecutionBlock — one `execution_round` reference row, rendered as the run
 * it points at (ADR-106 D3/D4/D8/D9).
 *
 * The chat holds no copy of the work. The row names a round (`instance|session|
 * round`) and everything shown here is read from that round's projection at
 * render time, through the Gateway's participant-checked block read (D10). The
 * visual language is the execution surface's `ExploreBlock`: a hairline card in
 * the agent's column, a quiet header line stating the outcome, content below.
 *
 * Three rules this component owns:
 * - "in progress" is positive evidence only — a `pending` projection (D9). An
 *   unreachable Runtime says "暂不可达" with a retry, never "still running";
 *   showing an outage as a live run lets a dead node rewrite history.
 * - the card a round is parked on lives HERE, in the block (D8). Answering it
 *   posts to the execution session's own write entry, where the owner check
 *   already runs; no chat row is written for the decision.
 * - an empty body still renders the centered outcome: how a round ended is a
 *   fact the transcript owes the reader even when there is nothing to show.
 */

import { memo, useState } from "react";
import {
  CircleAlert,
  CircleStop,
  Loader2,
  MessageCircleQuestion,
  Pause,
  ShieldCheck,
  Wrench,
} from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import { blockRefOf, type BlockCard } from "../../lib/chatBlocks";
import { useChatBlock, useChatBlockStore } from "../../stores/chatBlockStore";
import { useChatStore } from "../../stores/chatStore";
import { sendAnswer, sendApproval } from "../../lib/session-control";
import { ChatMarkdown } from "./ChatMarkdown";
import { ChatEventText } from "./ChatEventText";
import { ChatTurnStatusLine } from "./ChatTurnStatusLine";

/** Font size for block content: 90% of the app's, same as ExploreBlock's
 *  shell, so a chat run reads like the execution surface's own. */
const BLOCK_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";

interface ChatExecutionBlockProps {
  row: ChatMessageRow;
  /** The Chat this row belongs to — the block read is participation-checked
   *  against it, so it is part of the address, not a filter (D10). */
  chatSessionId: string;
  /** Drill into the execution session that owns this round. */
  onJumpToExec?: (agentInstanceId: string, execSid: string) => void;
  /** Address the composer to this round (D6 level 1) — what a `pause` card's
   *  "continue" and any follow-up on a finished round ride. */
  onReplyToRound?: (rowId: string, quote: string) => void;
  /** A viewer who is not this round's answerer (`answerable_by`): the question
   *  shows, the buttons do not (D10's read/write split). */
  canAnswer?: boolean;
  /** Who ran this round, shown in the header. The block sits in the speaker's
   *  column without its own avatar (the `ml-12` indent), so on a multi-party
   *  surface the run needs a name or several agents' blocks read as one. */
  authorName?: string;
  className?: string;
}

/** Outcome copy for a terminal round. `waiting` is terminal-looking but is not:
 *  it means the ball is in the human's court, which is what the card below the
 *  status line states. The settled outcomes reuse the turn-tier notice keys —
 *  "how a round ended" reads the same whether it is a status line or a block
 *  header, and a second copy of that copy would drift. */
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
    case "unknown":
      return "chatView.block.unknown";
    default:
      return "chatView.block.inProgress";
  }
}

function isFailure(status: string): boolean {
  return status === "failed" || status === "timeout";
}

export const ChatExecutionBlock = memo(function ChatExecutionBlock({
  row,
  chatSessionId,
  onJumpToExec,
  onReplyToRound,
  canAnswer = true,
  authorName,
  className,
}: ChatExecutionBlockProps) {
  const { t } = useTranslation();
  const ref = blockRefOf(row.meta);
  const entry = useChatBlock(chatSessionId, ref);
  const reload = useChatBlockStore((s) => s.reload);
  const projection = entry?.projection;
  const status = projection?.status ?? "";
  // D9: `pending` is the ONLY positive evidence that a round is still running —
  // the Gateway answers it from a live read of a non-terminal projection. A
  // terminal block is `ok` and immutable, and an `unreachable` one is an
  // outage, which must never be drawn as a run in flight.
  const inProgress = entry?.status === "pending";

  if (!ref || !entry) {
    // No coordinates, or the read has not landed: a placeholder of the block's
    // own height. A transcript that painted nothing here would let a missing
    // run look like a message nobody received.
    return (
      <ChatTurnStatusLine
        className={className}
        text={t(ref ? "chatView.block.loading" : "chatView.block.notFound")}
        danger={false}
      />
    );
  }

  if (entry.status === "unreachable") {
    // The transcript's event line with a retry: the fact is "we could not read
    // this", which belongs to the transcript, not to a bubble.
    return (
      <ChatEventText
        className={className}
        text={t("chatView.block.unreachable")}
        trailing={
          <button
            type="button"
            className="text-10 underline underline-offset-2"
            onClick={() => reload(chatSessionId, [ref])}
          >
            {t("chatView.block.retry")}
          </button>
        }
      />
    );
  }

  if (entry.status === "not_found" || !projection) {
    // The round names a session this chat cannot resolve. An explicit hole is
    // the honest answer (D5): inventing content would be writing history.
    return (
      <ChatTurnStatusLine
        className={className}
        text={t("chatView.block.notFound")}
        danger={false}
      />
    );
  }

  const instanceId = entry.agent_instance_id;
  const execSid = entry.agent_session_id;
  const answer = projection.answer?.body.trim() ?? "";
  const card = projection.card ?? null;

  return (
    <div className={`px-4 py-1 ${className ?? ""}`}>
      <div
        className="ml-12 max-w-[var(--content-max-width)] rounded-lg border border-hairline bg-surface/40 overflow-hidden"
        style={{ fontSize: BLOCK_FONT_SIZE }}
      >
        <header className="flex items-center gap-2 px-2.5 py-1.5 border-b border-hairline text-text-muted">
          {inProgress ? (
            <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin" />
          ) : isFailure(status) ? (
            <CircleAlert className="h-3.5 w-3.5 shrink-0 text-danger" />
          ) : status === "aborted" || status === "interrupted" ? (
            <CircleStop className="h-3.5 w-3.5 shrink-0" />
          ) : (
            <Wrench className="h-3.5 w-3.5 shrink-0" />
          )}
          {authorName ? (
            <span className="max-w-[10rem] shrink-0 truncate text-text-secondary">
              {authorName}
            </span>
          ) : null}
          <span className="truncate">{t(statusKey(inProgress ? "in_progress" : status))}</span>
          {row.meta?.task_id ? (
            <span className="shrink-0 rounded bg-surface/70 px-1.5 py-0.5 text-[0.8em] text-text-secondary">
              {row.meta.task_id}
            </span>
          ) : null}
          {onJumpToExec && execSid ? (
            <button
              type="button"
              className="ml-auto shrink-0 text-xs underline underline-offset-2"
              onClick={() => onJumpToExec(instanceId, execSid)}
            >
              {t("chatView.viewRun")}
            </button>
          ) : null}
        </header>

        {answer ? (
          <div className="px-2.5 py-2 text-text-primary">
            <ChatMarkdown content={answer} />
          </div>
        ) : inProgress ? (
          <div className="px-2.5 py-2 text-text-muted">{t("chatView.block.working")}</div>
        ) : null}

        {card ? (
          <BlockCardActions
            card={card}
            canAnswer={canAnswer}
            instanceId={instanceId}
            execSid={execSid}
            rowId={row.meta?.message_id ?? ""}
            onReplyToRound={onReplyToRound}
          />
        ) : null}
      </div>
    </div>
  );
});

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
 *  participant, which is why no chat-side copy is needed to close it. */
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

  // A pause has no execution-side request to resolve: the turn already ended
  // `waiting`, and the human's reply IS the next turn of this round (D6
  // level 1) — so it rides the composer, addressed to the reference row.
  if (kind === "pause") {
    return (
      <footer className="px-2.5 py-2 border-t border-hairline">
        <div className="flex items-center gap-1.5 text-text-secondary mb-1.5">
          <Icon className="h-3.5 w-3.5 shrink-0" />
          <span>{t(titleKey)}</span>
        </div>
        {card.prompt ? (
          <div className="mb-1.5 text-text-primary">
            <ChatMarkdown content={card.prompt} />
          </div>
        ) : null}
        {canAnswer && onReplyToRound ? (
          <button
            type="button"
            className="btn-accent px-2.5 py-1 rounded text-xs"
            onClick={() => onReplyToRound(rowId, card.prompt ?? "")}
          >
            {t("chatView.cardContinue")}
          </button>
        ) : null}
      </footer>
    );
  }

  const options = card.options ?? [];
  return (
    <footer className="px-2.5 py-2 border-t border-hairline">
      <div className="flex items-center gap-1.5 text-text-secondary mb-1.5">
        <Icon className="h-3.5 w-3.5 shrink-0" />
        <span>{t(titleKey)}</span>
      </div>
      {card.prompt ? (
        <div className="mb-1.5 text-text-primary">
          <ChatMarkdown content={card.prompt} />
        </div>
      ) : null}
      {!canAnswer ? (
        <p className="text-xs text-text-muted">{t("chatView.block.answeredByOwner")}</p>
      ) : (
        <div className="flex flex-wrap items-center gap-1.5">
          {kind === "approval" ? (
            <>
              <button
                type="button"
                disabled={busy}
                className="btn-accent px-2.5 py-1 rounded text-xs disabled:opacity-50"
                onClick={() =>
                  post(() => sendApproval(instanceId, execSid, { request_id: card.request_id, approved: true }))
                }
              >
                {t("chatView.cardApprove")}
              </button>
              <button
                type="button"
                disabled={busy}
                className="px-2.5 py-1 rounded text-xs border border-hairline disabled:opacity-50"
                onClick={() =>
                  post(() =>
                    sendApproval(instanceId, execSid, { request_id: card.request_id, approved: false }),
                  )
                }
              >
                {t("chatView.cardReject")}
              </button>
            </>
          ) : (
            options.map((opt) => (
              <button
                key={opt}
                type="button"
                disabled={busy}
                className="btn-accent px-2.5 py-1 rounded text-xs disabled:opacity-50"
                onClick={() =>
                  post(() => sendAnswer(instanceId, execSid, { request_id: card.request_id, answer: opt }))
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
                className="rounded border border-hairline bg-transparent px-1.5 py-1 text-xs"
              />
              <button
                type="button"
                disabled={busy || !freeText.trim()}
                className="px-2 py-1 rounded text-xs border border-hairline disabled:opacity-50"
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
            <span className="text-xs text-danger">{t("chatView.block.answerFailed")}</span>
          ) : null}
        </div>
      )}
    </footer>
  );
}
