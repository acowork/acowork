/**
 * InlineInteractionCard — the shared renderer for a durable
 * `interaction_request` card (design 27 §5.5.4).
 *
 * Unified visual language: header bar (icon + kind label + asker name)
 * above a content body (question + interaction). Two interaction forms:
 *
 *   - **List selection** (ask_question): radio list + "Other" + submit
 *   - **Action buttons** (approval / pause / others): click = immediate
 *
 * Three states per card:
 *   - Open: interactive, neutral hairline + neutral header
 *   - Answered: paints nothing — the answer bubble above quotes the question
 *     (see ReplyQuote), so a receipt here only repeated what is on screen
 *   - Closed without an answer: one quiet line in the card's slot, because
 *     "a decision was requested and nobody made it" exists nowhere else
 *
 * Layout strategy (aligned with agent bubbles):
 *   - ml-12 → same horizontal offset as agent bubble content (avatar + gap)
 *   - max-w-[var(--content-max-width)] → same width ceiling
 *   - width: fit-content → shrink-to-fit by content
 *
 * Answering rides the chat answer channel (`onAnswer` → a `user_message`
 * with `in_reply_to` = the card id); the Gateway resolves it against the
 * execution session (approval / answer), continuing the ORIGINAL turn.
 */

import { useState } from "react";
import { MessageCircleQuestion, ShieldCheck, Pause } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import type { ChatMessageRow } from "../../stores/chatSessionStore";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { truncateQuote } from "./ReplyQuote";
import { ChatMarkdown } from "./ChatMarkdown";

interface InlineInteractionCardProps {
  row: ChatMessageRow;
  /** Cards that can no longer be answered (§5.5.4). */
  closed: Set<string>;
  /** The subset the human actually replied to. An answered card paints
   *  nothing (the answer bubble quotes the question); anything else that
   *  closed — turn ended, timeout, stop — leaves the quiet one-line note. */
  answered: Set<string>;
  /** Resolved display name of the asking agent (the group resolves the
   *  participant; a 1:1 passes its single agent's name). */
  askerName?: string;
  /** Admin viewing a session they never joined: the question is shown,
   *  never answerable — posting into someone else's conversation stays
   *  forbidden. */
  readOnly?: boolean;
  /** Answer the card. `immediate=true` → the chosen option text is sent now;
   *  `immediate=false` → a free-text answer: the caller focuses its composer
   *  instead of sending an empty message. */
  onAnswer: (row: ChatMessageRow, text: string, immediate: boolean) => void;
}

// ── Kind → header config mapping ──────────────────────────────────

/**
 * Per-kind chrome. Every card outline is the neutral hairline and every
 * header title the regular text tier — same recipe as CodeBlock's title bar.
 * `--color-accent` is reserved for button backgrounds (globals.css
 * `.btn-accent` / `.bg-accent-strong`) and nothing else paints the highlight;
 * the former emerald / amber semantic borders on approval / pause were
 * unified away per user decision (2025, chat-card style). Kinds are told
 * apart by icon + title, not by color.
 */
interface KindConfig {
  icon: React.ReactNode;
  titleKey: string;
}

function kindConfig(kind: string): KindConfig {
  switch (kind) {
    case "approval":
      return {
        icon: <ShieldCheck className="h-3.5 w-3.5 shrink-0" />,
        titleKey: "chatView.cardApproval",
      };
    case "pause":
      return {
        icon: <Pause className="h-3.5 w-3.5 shrink-0" />,
        titleKey: "chatView.cardPause",
      };
    default: // ask_question and unknown kinds
      return {
        icon: <MessageCircleQuestion className="h-3.5 w-3.5 shrink-0" />,
        titleKey: "chatView.cardAsk",
      };
  }
}

/** Action button labels for approval / pause kinds. These double as the
 *  answer text sent to the Gateway — `is_affirmative()` recognises them. */
const APPROVAL_ACTIONS = [
  { label: "chatView.cardApprove", text: "\u6279\u51c6", tone: "accent" as const },
  { label: "chatView.cardReject", text: "\u62d2\u7edd", tone: "solid" as const },
];
const PAUSE_ACTIONS = [
  { label: "chatView.cardContinue", text: "\u7ee7\u7eed", tone: "accent" as const },
  { label: "chatView.cardStop", text: "\u505c\u6b62", tone: "solid" as const },
];

export function InlineInteractionCard({
  row,
  closed,
  answered,
  askerName,
  readOnly,
  onAnswer,
}: InlineInteractionCardProps) {
  const { t } = useTranslation();
  const it = row.meta?.interaction;
  const kind = it?.kind ?? "ask_question";
  const question = it?.question || row.body;
  const mid = row.meta?.message_id ?? "";

  // --- Local selection state for list mode ---
  const [selected, setSelected] = useState<string | null>(null);

  const cfg = kindConfig(kind);
  const isListMode = kind === "ask_question" && (it?.options?.length ?? 0) > 0;
  const actions = kind === "approval" ? APPROVAL_ACTIONS : kind === "pause" ? PAUSE_ACTIONS : null;
  // Two-button action cards (approval / pause) ride a narrow clamp: the tool
  // text wraps to a couple of lines instead of stretching the card to the
  // full content width while the buttons stay tiny — the balance the user
  // asked for. List/option cards keep the shared content-max-width ceiling.
  const widthCls = actions ? "max-w-sm" : "max-w-[var(--content-max-width)]";

  // ── Closed cards ──
  if (closed.has(mid)) {
    // Answered: paint nothing. The receipt repeated the question in a full card
    // shell (header band, hairline, padding) directly above the answer, and
    // carried nothing else — not the answer text, not who gave it. Both halves
    // of that fact are on screen without it: the question was just read in the
    // open card, the answer is the bubble below, and that bubble now quotes the
    // question (§5.5.4 puts the record on the `in_reply_to` row, not on a card).
    if (answered.has(mid)) return null;
    // Closed with nobody answering — the turn ended, timed out, or was stopped.
    // This is the one fact the transcript would otherwise lose: a decision was
    // requested and never got one, and the agent moved on. It stays visible, but
    // as a quiet line in the card's own slot rather than a card shell, because
    // it is a status note, not an utterance.
    return (
      <div
        className="ml-12 my-1 flex max-w-[var(--content-max-width)] items-center gap-1.5 text-10 text-text-tertiary"
        title={question}
      >
        {cfg.icon}
        <span className="min-w-0 truncate">
          {t("chatView.cardUnanswered", { question: truncateQuote(question) })}
        </span>
      </div>
    );
  }

  // ── Read-only (admin viewing) ──
  if (readOnly) {
    return (
      <div className={`ml-12 my-2 ${widthCls} w-fit`}>
        <div className="overflow-hidden rounded-md border border-border-outer">
          <div className="flex items-center gap-[7px] border-b border-border-divider bg-chat-title px-3 py-[7px] text-xs font-medium text-text">
            {cfg.icon}
            <span>{t(cfg.titleKey)}</span>
            {askerName && <span className="ml-auto text-10 font-normal opacity-70">{askerName}</span>}
          </div>
          <div className="bg-chat-bubble px-3 py-2.5">
            {/* Markdown, not a bare text node: the question is model output and
                carries inline code and links. Routed through the shared chat
                renderer so the `prose` scope -- and every table and link rule
                globals.css hangs under it -- cannot be forgotten the way the
                per-surface bubble copies drifted. */}
            <ChatMarkdown content={question} />
          </div>
        </div>
      </div>
    );
  }

  // ── Open / interactive state ──
  return (
    <div className={`ml-12 my-2 ${widthCls} w-fit`}>
      <div className="overflow-hidden rounded-md border border-border-outer">
        {/* Header bar */}
        <div className="flex items-center gap-[7px] border-b border-border-divider bg-chat-title px-3 py-[7px] text-xs font-medium text-text">
          {cfg.icon}
          <span>{t(cfg.titleKey)}</span>
          {askerName && <span className="ml-auto text-10 font-normal opacity-70">{askerName}</span>}
        </div>

        {/* Body */}
        <div className="bg-chat-bubble px-3 py-2.5">
          <div className="mb-2">
            <ChatMarkdown content={question} />
          </div>

          {/* Form A: list selection (ask_question with options) */}
          {isListMode && (
            <ListSelection
              options={it!.options!}
              selected={selected}
              onSelect={setSelected}
              onSubmit={(text) => onAnswer(row, text, true)}
              onFreeAnswer={() => onAnswer(row, "", false)}
            />
          )}

          {/* Form B: action buttons (approval / pause / no-options) */}
          {!isListMode && (
            <div className="flex flex-wrap gap-2">
              {actions ? (
                actions.map((a) => (
                  <button
                    key={a.label}
                    onClick={() => onAnswer(row, a.text, true)}
                    className={`rounded px-3 py-1.5 text-xs ${a.tone === "accent" ? "btn-accent" : "btn-solid"}`}
                  >
                    {t(a.label)}
                  </button>
                ))
              ) : it?.options && it.options.length > 0 ? (
                // Unknown kind with options → buttons for each option
                <>
                  {it.options.map((opt) => (
                    <button
                      key={opt}
                      onClick={() => onAnswer(row, opt, true)}
                      className="rounded btn-accent px-3 py-1.5 text-xs"
                    >
                      {opt}
                    </button>
                  ))}
                  <button
                    onClick={() => onAnswer(row, "", false)}
                    className="rounded btn-solid px-3 py-1.5 text-xs"
                  >
                    {t("chatView.freeAnswer")}
                  </button>
                </>
              ) : (
                // No options at all → single answer button
                <button
                  onClick={() => onAnswer(row, "", false)}
                  className="rounded btn-accent px-3 py-1.5 text-xs"
                >
                  {t("chatView.answer")}
                </button>
              )}
            </div>
          )}
        </div>
      </div>
    </div>
  );
}

// ── List Selection sub-component ──────────────────────────────────

function ListSelection({
  options,
  selected,
  onSelect,
  onSubmit,
  onFreeAnswer,
}: {
  options: string[];
  selected: string | null;
  onSelect: (v: string | null) => void;
  onSubmit: (text: string) => void;
  onFreeAnswer: () => void;
}) {
  const { t } = useTranslation();
  const isOther = selected === "__other__";
  const canSubmit = selected !== null && !isOther;

  return (
    <>
      <ul className="mb-2 space-y-0.5">
        {options.map((opt) => (
          <li
            key={opt}
            className={`flex items-center gap-2 rounded px-2 py-1 text-xs cursor-pointer transition-colors ${selected === opt ? listRowSelected : listRowHover}`}
            onClick={() => onSelect(opt)}
          >
            <input
              type="radio"
              name="card-option"
              checked={selected === opt}
              onChange={() => onSelect(opt)}
              className="h-3 w-3 shrink-0"
              style={{ accentColor: "var(--color-accent)" }}
            />
            <span className="font-medium">{opt}</span>
          </li>
        ))}
        {/* "Other" — triggers free-text via composer */}
        <li
          className={`flex items-center gap-2 rounded px-2 py-1 text-xs cursor-pointer transition-colors ${isOther ? listRowSelected : listRowHover}`}
          onClick={() => onSelect("__other__")}
        >
          <input
            type="radio"
            name="card-option"
            checked={isOther}
            onChange={() => onSelect("__other__")}
            className="h-3 w-3 shrink-0"
            style={{ accentColor: "var(--color-accent)" }}
          />
          <span className="font-medium">{t("askQuestionCard.other")}</span>
        </li>
      </ul>
      <div className="flex items-center gap-2">
        <button
          disabled={!canSubmit}
          onClick={() => canSubmit && onSubmit(selected!)}
          className="rounded btn-accent px-3 py-1.5 text-xs disabled:opacity-40 disabled:cursor-not-allowed"
        >
          {t("askQuestionCard.submit")}
        </button>
        <button
          onClick={onFreeAnswer}
          className="rounded btn-solid px-3 py-1.5 text-xs"
        >
          {t("chatView.freeAnswer")}
        </button>
      </div>
    </>
  );
}
