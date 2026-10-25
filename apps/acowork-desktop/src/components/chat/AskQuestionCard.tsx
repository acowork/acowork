import { useState } from "react";
import { MessageCircleQuestion } from "lucide-react";
import type { AskQuestionEvent } from "../../lib/types";
import { StyledTextarea } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import { useChatStore } from "../../stores/chatStore";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";
import { ChatMarkdown } from "./ChatMarkdown";

interface AskQuestionCardProps {
  event: AskQuestionEvent;
  agentId: string;
  sessionId?: string | null;
  onAnswer: (requestId: string, answer: string) => void;
}

/**
 * AskQuestionCard — the execute-session interaction card.
 *
 * Unified visual language with InlineInteractionCard: header bar (icon +
 * title + countdown) + body (question + radio list + submit). Layout
 * strategy matches agent messages (ml-12 + content-max-width clamp).
 */
export function AskQuestionCard({ event, agentId, sessionId, onAnswer }: AskQuestionCardProps) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<string | null>(null);
  const [otherText, setOtherText] = useState("");
  const [submitted, setSubmitted] = useState(false);

  // Countdown from the runtime's 5s heartbeat (ToolProgress channel).
  const progress = useChatStore((s) =>
    agentId && sessionId
      ? s.agentStates[agentId]?.sessionStates[sessionId]?.toolProgress?.[event.request_id]
      : undefined,
  );
  const fallbackMs = (event.timeout_seconds ?? 0) * 1000;
  const elapsedMs = progress?.elapsedMs ?? 0;
  const timeoutMs = progress?.timeoutMs ?? fallbackMs;
  const remainingSecs: number | null =
    !timeoutMs ? null : Math.max(0, Math.ceil((timeoutMs - elapsedMs) / 1000));

  const isOtherSelected = selected === "__other__";
  const canSubmit = !submitted && selected !== null && (!isOtherSelected || otherText.trim().length > 0);
  const countdownLabel = remainingSecs !== null && remainingSecs > 0
    ? `${Math.floor(remainingSecs / 60)}:${String(remainingSecs % 60).padStart(2, "0")}`
    : remainingSecs === 0 ? "expired" : null;

  const handleSubmit = () => {
    if (!canSubmit) return;
    const answer = isOtherSelected ? otherText.trim() : selected!;
    setSubmitted(true);
    onAnswer(event.request_id, answer);
  };

  return (
    <div className="ml-12 my-1.5 max-w-[var(--content-max-width)] w-fit">
      <div className="overflow-hidden rounded-md border border-border-outer">
        {/* Header bar */}
        <div className="flex items-center gap-[7px] border-b border-border-divider bg-chat-title px-3 py-[7px] text-xs font-medium text-text">
          <MessageCircleQuestion className="h-3.5 w-3.5 shrink-0" />
          <span className="flex-1">{t("chatView.cardAsk")}</span>
          {countdownLabel && countdownLabel !== "expired" && (
            <span
              className="text-10 font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right"
              aria-label={t("askQuestionCard.expiresIn")}
            >
              {countdownLabel}
            </span>
          )}
          {countdownLabel === "expired" && (
            <span className="text-10 font-mono text-text-tertiary shrink-0">
              {t("askQuestionCard.expired")}
            </span>
          )}
        </div>

        {/* Body */}
        <div className="bg-chat-bubble px-3 py-2.5">
          {event.title && (
            <div className="mb-1 text-xs font-medium text-text-secondary">{event.title}</div>
          )}
          {/* Same renderer as the durable chat card, so one question reads
              identically on the execution surface and in chat. */}
          <div className="mb-2">
            <ChatMarkdown content={event.question} />
          </div>

          {/* Options list */}
          <ul className="mb-2 space-y-0.5">
            {(event.options ?? []).map((opt, idx) => (
              <li
                key={idx}
                className={`flex items-center gap-2 rounded px-2 py-1 text-xs transition-colors cursor-pointer
                  ${submitted ? "opacity-60 pointer-events-none" : listRowHover}
                  ${selected === opt.label ? listRowSelected : ""}`}
                onClick={() => !submitted && setSelected(opt.label)}
              >
                <input
                  type="radio"
                  name={`question-${event.request_id}`}
                  value={opt.label}
                  checked={selected === opt.label}
                  disabled={submitted}
                  onChange={() => setSelected(opt.label)}
                  className="shrink-0 h-3 w-3"
                  style={{ accentColor: "var(--color-accent)" }}
                />
                <span className="font-medium text-text">{opt.label}</span>
                {opt.description && (
                  <span className="text-text-tertiary">— {opt.description}</span>
                )}
              </li>
            ))}

            {/* "Other" option */}
            <li
              className={`flex items-center gap-2 rounded px-2 py-1 text-xs transition-colors cursor-pointer
                ${submitted ? "opacity-60 pointer-events-none" : listRowHover}
                ${isOtherSelected ? listRowSelected : ""}`}
              onClick={() => !submitted && setSelected("__other__")}
            >
              <input
                type="radio"
                name={`question-${event.request_id}`}
                value="__other__"
                checked={isOtherSelected}
                disabled={submitted}
                onChange={() => setSelected("__other__")}
                className="shrink-0 h-3 w-3"
                style={{ accentColor: "var(--color-accent)" }}
              />
              <span className="font-medium text-text">{t("askQuestionCard.other")}</span>
            </li>
          </ul>

          {/* Other textarea */}
          {isOtherSelected && !submitted && (
            <StyledTextarea
              className="mb-2 ml-4"
              rows={2}
              placeholder={t("askQuestionCard.placeholder")}
              value={otherText}
              onChange={(e) => setOtherText(e.target.value)}
              autoFocus
            />
          )}

          {/* Submit */}
          <div className="flex items-center gap-2">
            <button
              disabled={!canSubmit}
              onClick={handleSubmit}
              className="rounded btn-accent px-3 py-1.5 text-xs disabled:opacity-40 disabled:cursor-not-allowed"
            >
              {submitted ? t("askQuestionCard.submitted") : t("askQuestionCard.submit")}
            </button>
            {submitted && (
              <span className="text-10 text-text-tertiary">
                Answer sent
              </span>
            )}
          </div>
        </div>
      </div>
    </div>
  );
}
