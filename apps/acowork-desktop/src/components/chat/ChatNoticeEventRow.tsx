/**
 * ChatNoticeEventRow — the shared renderer for `system_notice` chat rows.
 *
 * The channel facts, never utterances, so they paint as a quiet line and carry
 * no avatar:
 * - `budget_exhausted` / `truncated` / `unavailable`: a limit the round hit.
 *
 * `delivery_failed` used to belong here and no longer does (ADR-105 D5,
 * revision 2026-11-06): a failed send is news, not history. The Gateway writes
 * no receipt row for it any more — the failure is served live as
 * `delivery.alert` on the row the send owed and painted by `DeliveryAlertLine`.
 * Rows carrying the old `notice_type` are retired in place (`RETIRED_NOTICES`),
 * so they disappear from existing transcripts instead of lingering forever.
 *
 * The third kind is not a channel fact at all. `session_choice` / `confirm_continue`
 * is an addressing question (ADR-106 D6 levels 3 and 5) the AGENT asks about its
 * own runs: empty-bodied, its options and its answered-ness arriving as the
 * service's projection (`row.delivery.question`, design 31 §6) so the surfaces
 * render that verbatim instead of reading `meta` and re-deriving whether the
 * question is still open.
 *
 * IDENTITY BELONGS TO THE LINE, NOT TO THE CARD. The same question is drawn in
 * two places, and only one of them is the agent's own line:
 *  - a standalone transcript row (`identity="own"`): nothing above it names the
 *    author, so the card wears the agent's avatar + name at the 40px gutter the
 *    execution block uses — the answer then replaces the question under one
 *    name, and the exchange reads as one continuous turn;
 *  - the tail of the run it asks about (`identity="inherited"`): the block
 *    already painted that avatar, that name and that gutter, so the card adds
 *    none of them. It starts exactly where the block's content starts.
 *  - the bottom dock (`identity="floating"`): the question is parked over the
 *    composer's edge, outside the transcript altogether. No line owns it, so it
 *    wears the author's face and name itself — but no 40px gutter, because
 *    there is no bubble column to line up with.
 * Painting the header unconditionally is the 2026-11-06 defect: a card hung on a
 * block arrived with a second face over the same question, its body 64px (px-4 +
 * gap + the 40px gutter) to the right of the block's own content — neither a
 * standalone card nor a block part. Which one it is, is the HOST's fact, so the
 * host states it, and the prop has no default: a surface that has not decided
 * where its card sits has not decided who speaks.
 *
 * Everything the runtime used to write as progress (dispatch / round-open
 * milestones, the router-fallback receipt, the card-resolution receipt) is gone
 * from the stream: the log records conversation, and a card is no longer a chat
 * row at all (D8) — its answer is an execution-side fact shown inside the block.
 */

import { useState } from "react";
import {
  ChevronRight,
  CornerDownRight,
  ListTree,
  Loader2,
  Plus,
} from "lucide-react";
import { ChatEventText } from "./ChatEventText";
import { InteractionCardShell } from "./InteractionCardShell";
import { AgentAvatar } from "../common/AgentAvatar";
import { useTranslation } from "../../i18n/useTranslation";
import { RETIRED_NOTICES } from "../../lib/retiredNotices";
import { isCardNotice } from "../../lib/chatCardAnswer";
import { cn } from "../../lib/utils";
import type { CardChooseHandler, CardChoice } from "../../lib/chatCardAnswer";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import { useAgentStore } from "../../stores/agentStore";

/** How long a click keeps the card busy before the buttons come back. The card
 *  normally leaves the transcript on its own — the projection flips it to
 *  `answered` the moment a round threads back to the message it asked about —
 *  so this is only a backstop: a projection that never arrives must not leave
 *  the human with a dead, permanently disabled question. */
const CHOICE_PENDING_MAX_MS = 20_000;

/** The candidate's state as a dot on its row (design 29 §5.2 vocabulary). A
 *  card must not claim a status it was not told: an option that arrived without
 *  one gets a neutral dot, never "idle". */
function statusTone(status?: string | null): string {
  switch (status) {
    case "working":
      return "bg-amber-500 animate-pulse";
    case "idle":
      return "bg-emerald-500";
    case "errored":
      return "bg-red-500";
    default:
      return "bg-zinc-400 dark:bg-zinc-600";
  }
}

interface ChatNoticeEventRowProps {
  row: ChatMessageRow;
  /** Surface column gutter, for a line that is not the card's own (a channel
   *  receipt, `ChatEventText`'s centred line). On the CARD it composes AFTER the
   *  identity's `px-4`, so a host that passes its own gutter wins (`cn` is a
   *  tailwind merge) — `identity` sets the default, the host keeps the last word. */
  className?: string;
  /** Answering an addressing card. Absent means the row is not answerable here:
   *  a read-only viewer never gets the action (ADR-106 D10 — only the person who
   *  spoke may decide where their words go), and the option buttons are part of
   *  the answer, not decoration. The choice is turned into a re-delivery of the
   *  human's own utterance by `chatCardAnswer` (D6), and the result is what keeps
   *  the card honest while that happens (see `CardChooseResult`). */
  onChoose?: CardChooseHandler;
  /** The card author's display name, when the surface resolves names from a
   *  roster instead of the agent catalog (the group panel does). Same
   *  precedence as `ChatExecutionBlock`: this first, the catalog second, the
   *  instance id last. */
  authorName?: string;
  /** Who owns the identity over this card — the line it sits on, never the card
   *  itself. `"own"`: it IS a transcript line (a standalone row), so it paints
   *  the agent's avatar + name and the 40px gutter above the question.
   *  `"inherited"`: it hangs at the tail of the run it asks about, whose block
   *  already painted all three, so the card adds none and starts where the
   *  block's content starts. `"floating"`: it is docked over the composer's
   *  edge, outside the transcript — no line owns it, so it names its author
   *  itself, but it wears no gutter (there is no bubble column to line up
   *  with). Required, and deliberately so: a second identity over one question
   *  ("one card, two faces") is the defect this states, and a surface that has
   *  not decided where its card sits must not guess. */
  identity: "own" | "inherited" | "floating";
}

export function ChatNoticeEventRow({
  row,
  className,
  authorName,
  identity,
  onChoose,
}: ChatNoticeEventRowProps) {
  const { t } = useTranslation();
  const noticeType = row.meta?.notice_type ?? "";
  // Hooks stay above every early return: a row's notice type never changes, but
  // the renderer's call order is not allowed to depend on that.
  const author = useAgentStore((s) => s.agents[row.from]?.meta);
  /** The option the human just pressed, while the click is still becoming a
   *  delivery. Non-null = the card is busy: the pressed row spins, everything
   *  else goes dead, and the card itself stays put — the human's decision must
   *  not blink out from under the pointer. */
  const [pending, setPending] = useState<CardChoice | null>(null);

  // Retired milestones: history only.
  if (RETIRED_NOTICES.has(noticeType)) return null;
  // An addressing question (ADR-106 D6 levels 3 and 5) — a decision card, and
  // it has to look like one: a hairline shell with a header, full-width option
  // rows that read as buttons (label, the router's reason, a status dot, a
  // chevron), and one quiet escape. The language is `AskQuestionCard`'s, so the
  // same question does not change costume between the chat surface and the
  // execution surface.
  if (isCardNotice(noticeType)) {
    const confirm = noticeType === "confirm_continue";
    const question = t(
      confirm ? "chatView.block.confirmSession" : "chatView.block.chooseSession",
    );
    // The service's projection (design 31 §6): the options the question offers,
    // whether a round already answered it, and which session took the answer.
    const card = row.delivery?.question;
    const options = card?.options ?? [];
    const answered = card?.answered === true;
    // A card with nothing to offer is retired chrome, not a question: the rows
    // that reach a surface like this are pre-projection leftovers (no options,
    // nothing to click), and painting them as a centered sentence in the middle
    // of the transcript made a bare "Continue this task?" look like the agent
    // saying something it never said.
    if (options.length === 0) return null;

    // An answer must NAME what it continued. The payload carries the
    // candidate's own title; the generic wording is only the fallback for an
    // option that arrived with no label at all.
    const labelOf = (option: (typeof options)[number]) =>
      option.label?.trim() ||
      option.session_id?.trim().slice(0, 8) ||
      t("chatView.block.chooseThis");

    // Settled: paint nothing. A card is a question, and an answered question is
    // an intermediate state of that turn, not a record of the conversation —
    // ADR-106 D14 keeps history on disk and hides it at render time, and where
    // the words went is already told by the round's block. Leaving a receipt
    // here stated the same fact a second time, one identical line per test
    // message, floating with no subject of its own.
    //
    // This is also what makes the card-to-block handover continuous: the moment
    // the round threads back to this message the projection flips `answered`,
    // the card unmounts, and `ChatExecutionBlock` takes the slot directly under
    // the SAME avatar and name.
    //
    // `!answerable` leaves by the same door and for the same reason: the card
    // names a row that is not in the conversation, so no click can ever land
    // (`GET …/answer` answers `origin_gone`/422) and the question is not a live
    // one. That is a durable fact about rows an older build wrote — the log is
    // append-only — so the surface drops them at render time instead of showing
    // a list of dead options, and instead of the toast a click used to raise.
    // Absent means the service did not say: treat it as answerable.
    if (answered || card?.answerable === false) return null;

    // The card's author IS the agent whose runs are on offer, so the identity is
    // the agent instance on the row — resolved exactly like the execution block
    // resolves it, so the header cannot shift when the card becomes the block.
    // One name per participant: the author's own `name`, else the raw id.
    const displayName = authorName?.trim() || author?.name || row.from;

    /** Is this option the one being delivered right now? */
    const isPendingOption = (sessionId?: string) =>
      pending !== null && !!sessionId && pending.sessionId === sessionId;
    const busy = pending !== null;

    /**
     * Turn a click into a delivery and keep the card honest while it happens.
     *
     * A landed click is NOT released here: the card leaves by being answered
     * (above), and releasing early would flash a live question under the
     * human's pointer after they had already decided. A click that delivered
     * nothing re-opens the card, with the only backstop being a bounded wait for
     * a projection that may never come.
     */
    const choose = (choice: CardChoice) => {
      if (!onChoose || busy) return;
      setPending(choice);
      const settle = (landed: boolean) => {
        if (!landed) {
          setPending(null);
          return;
        }
        window.setTimeout(() => setPending(null), CHOICE_PENDING_MAX_MS);
      };
      let outcome: void | boolean | Promise<void | boolean>;
      try {
        outcome = onChoose(choice);
      } catch {
        settle(false);
        return;
      }
      Promise.resolve(outcome).then(
        (v) => settle(v !== false),
        () => settle(false),
      );
    };

    // The shared card chrome — the same shell `AskQuestionCard` wears, so a
    // question does not change costume between surfaces. The body is a
    // full-width ruled list, so the card's own surface replaces the default
    // bubble fill.
    const cardBody = (
      <InteractionCardShell
        icon={confirm ? CornerDownRight : ListTree}
        title={question}
        className="w-fit min-w-[18rem] max-w-[32rem]"
        bodyClassName="bg-surface p-0"
        headerRight={
          onChoose ? (
            <span className="shrink-0 text-10 font-normal text-text-tertiary">
              {busy ? t("chatView.block.choosing") : t("chatView.block.unanswered")}
            </span>
          ) : undefined
        }
      >
        {/* One row per candidate. The whole row is the button: a bordered
            box holding two lines of text reads as a disabled field, not as
            something to press. */}
        <div className="divide-y divide-border-divider">
          {options.map((option) => {
            const label = labelOf(option);
            const why = option.why?.trim();
            const sending = isPendingOption(option.session_id);
            const row = (
              <>
                {sending ? (
                  <Loader2 className="h-3 w-3 shrink-0 animate-spin text-text-tertiary" />
                ) : (
                  <span
                    className={cn(
                      "h-1.5 w-1.5 shrink-0 rounded-full",
                      statusTone(option.status),
                    )}
                    aria-hidden="true"
                  />
                )}
                <span className="min-w-0 flex-1">
                  <span className="block truncate text-11 font-medium text-text">
                    {label}
                  </span>
                  {why && (
                    <span className="block truncate text-10 text-text-tertiary">
                      {why}
                    </span>
                  )}
                </span>
              </>
            );
            // A viewer who may not decide (ADR-106 D10) sees the options
            // but not a button: the question is a fact about the channel,
            // the click is the speaker's.
            if (!onChoose) {
              return (
                <div
                  key={option.session_id}
                  className="flex items-center gap-2 px-2.5 py-1.5"
                  title={why ? `${label} — ${why}` : label}
                >
                  {row}
                </div>
              );
            }
            return (
              <button
                key={option.session_id}
                type="button"
                disabled={busy}
                onClick={() => choose({ sessionId: option.session_id })}
                title={why ? `${label} — ${why}` : label}
                className="group flex w-full items-center gap-2 px-2.5 py-1.5 text-left transition-colors hover:bg-nav-item-hover focus-visible:outline-none focus-visible:ring-1 focus-visible:ring-[var(--color-accent)] disabled:cursor-default disabled:opacity-50 disabled:hover:bg-transparent"
              >
                {row}
                {!sending && (
                  <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-tertiary transition-transform group-hover:translate-x-0.5" />
                )}
              </button>
            );
          })}
        </div>
        {/* D6's cheap escape, demoted to a footer action: "new task" is the
            way OUT of the suggestion, never a second candidate. */}
        {card?.allow_new_task && onChoose && (
          <div className="border-t border-border-divider bg-surface">
            <button
              type="button"
              disabled={busy}
              onClick={() => choose({ forceNew: true })}
              className="flex w-full items-center gap-2 px-2.5 py-1.5 text-left text-11 text-text-tertiary transition-colors hover:bg-nav-item-hover hover:text-text-secondary disabled:cursor-default disabled:opacity-50 disabled:hover:bg-transparent"
            >
              {pending?.forceNew ? (
                <Loader2 className="h-3.5 w-3.5 shrink-0 animate-spin" />
              ) : (
                <Plus className="h-3.5 w-3.5 shrink-0" />
              )}
              {t("chatView.block.newTask")}
            </button>
          </div>
        )}
        {!onChoose && (
          <div className="border-t border-border-divider px-2.5 py-1.5 text-10 text-text-tertiary">
            {t("chatView.block.answeredByOwner")}
          </div>
        )}
      </InteractionCardShell>
    );

    // The identity over a card belongs to whatever OWNS the line it sits on —
    // the rule the file comment states. Standing in the transcript the card is
    // the agent's own line, so it wears the agent's avatar, name and gutter.
    // Hung at a run's tail it wears neither: the block above already said who
    // is asking, and a second face over the same question is how one card ended
    // up duplicated and inset into the middle of the block's own column.
    // Docked at the bottom it owns no line at all, so it names its author
    // itself — but keeps the gutter out of it: there is no bubble column there
    // to line up with.
    const namesAuthor = identity === "own" || identity === "floating";
    return (
      <div
        className={cn("py-1", identity === "own" && "px-4", className)}
        data-testid="chat-card-options"
      >
        {namesAuthor && (
          <div
            data-testid="chat-card-author"
            className="mb-2 mt-1 flex items-center gap-2"
          >
            <AgentAvatar
              agentId={row.from}
              displayName={displayName}
              avatarUrl={author?.avatar ?? null}
              version={author?.version}
              builtinAvatarId={author?.builtin_avatar ?? null}
              size={40}
              className="shrink-0"
            />
            <div className="flex flex-col">
              <span className="text-xs font-medium text-text-tertiary">
                {displayName}
              </span>
              {author?.role && (
                <span className="text-10 leading-tight text-text-tertiary">
                  {author.role}
                </span>
              )}
            </div>
          </div>
        )}
        {identity === "own" ? (
          // The same 40px gutter the agent bubble and the execution block use,
          // so the question and the run that follows start on one line.
          <div className="flex items-start gap-2">
            <div className="w-10 shrink-0" />
            <div className="flex min-w-0 flex-1 flex-col items-start">
              {cardBody}
            </div>
          </div>
        ) : (
          // In a block's column: the host owns the gutter, so the card starts
          // exactly where the block's own content starts.
          <div className="flex min-w-0 flex-col items-start">{cardBody}</div>
        )}
      </div>
    );
  }
  // A channel receipt has no subject: `budget_exhausted` / `truncated` /
  // `unavailable` report a limit of the channel, and the request they name never
  // became a row (design 31 §9-3). So it is a centered line about the transcript.
  // The one receipt that DID own a row (`delivery_failed`) is retired above —
  // live delivery failures are painted by `DeliveryAlertLine`.
  const text = row.body || row.meta?.status || "";
  if (!text) return null;
  return <ChatEventText text={text} className={className} />;
}
