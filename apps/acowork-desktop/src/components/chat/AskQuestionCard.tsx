import { useState } from "react";
import { MessageCircleQuestion } from "lucide-react";
import type { AskQuestionEvent } from "../../lib/types";
import { StyledTextarea } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import { useChatStore } from "../../stores/chatStore";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";

interface AskQuestionCardProps {
  event: AskQuestionEvent;
  agentId: string;
  sessionId?: string | null;
  onAnswer: (requestId: string, answer: string) => void;
}

/**
 * AskQuestionCard: renders an ask_user_question prompt with options + "Other" textarea.
 *
 * Design:
 * - Shows question title (if present) and question text
 * - Options rendered as radio buttons with optional descriptions
 * - Last option is always "Other" which reveals a textarea for free-text input
 * - Submit button uses accent color, inline with the card (no modal/dialog)
 * - Disabled after submission
 * - Countdown badge is driven by the runtime's 5s heartbeat (re-uses the
 *   tool-progress channel keyed by `request_id`) so it cannot drift out
 *   of sync with the backend's 5-minute wall-clock timeout.
 */
export function AskQuestionCard({ event, agentId, sessionId, onAnswer }: AskQuestionCardProps) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<string | null>(null);
  const [otherText, setOtherText] = useState("");
  const [submitted, setSubmitted] = useState(false);

  // Countdown timer for question wait timeout.
  // Source of truth = backend wall-clock. The runtime emits
  // ChunkEvent::ToolProgress (reused per ADR-045) every 5s while waiting
  // for an answer, keyed by `request_id`. We read the same
  // `toolProgress[request_id]` map ExploreBlock uses for tool execution
  // progress, so the on-screen countdown stays in lock-step with the
  // backend's 5-minute timeout even when the renderer throttles timers.
  // Falls back to `event.timeout_seconds` for the first 5s (before the
  // first heartbeat lands) and to "expired" once elapsed >= timeout.
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
  const canSubmit = !submitted && (selected !== null) && (!isOtherSelected || otherText.trim().length > 0);
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
    <div
      className="my-1.5 max-w-[var(--content-max-width)] rounded-md border border-border-outer bg-zinc-50 px-3 py-2  dark:bg-zinc-800/40"
    >
      {/* Header */}
      <div className="flex items-start gap-1.5 mb-1.5">
        <MessageCircleQuestion
          className="h-3.5 w-3.5 shrink-0 mt-0.5 text-text-tertiary "
        />
        <div className="min-w-0 flex-1">
          {event.title && (
            <div className="text-xs font-medium text-text  mb-0.5">
              {event.title}
            </div>
          )}
          <div className="text-xs text-text-secondary ">
            {event.question}
          </div>
        </div>
        {/* Countdown badge — mirrors approval-flow timer in ExploreBlock */}
        {countdownLabel && countdownLabel !== "expired" && (
          <span
            className="text-10 font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right mt-0.5"
            aria-label={t("askQuestionCard.expiresIn")}
          >
            {countdownLabel}
          </span>
        )}
        {countdownLabel === "expired" && (
          <span className="text-10 font-mono text-text-tertiary  shrink-0 mt-0.5">
            {t("askQuestionCard.expired")}
          </span>
        )}
      </div>

      {/* Options */}
      <div className="ml-5 space-y-0.5">
        {(event.options ?? []).map((opt, idx) => (
          <label
            key={idx}
            className={`flex items-center gap-1.5 rounded px-2 py-1 text-xs transition-colors cursor-pointer
              ${submitted ? "opacity-60 pointer-events-none" : listRowHover}
              ${selected === opt.label ? listRowSelected : ""}`}
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
            <span className="font-medium text-text ">{opt.label}</span>
            {opt.description && (
              <span className="text-text-tertiary ">— {opt.description}</span>
            )}
          </label>
        ))}

        {/* Other option */}
        <label
          className={`flex items-center gap-1.5 rounded px-2 py-1 text-xs transition-colors cursor-pointer
            ${submitted ? "opacity-60 pointer-events-none" : listRowHover}
            ${isOtherSelected ? listRowSelected : ""}`}
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
          <span className="font-medium text-text ">{t("askQuestionCard.other")}</span>
        </label>

        {/* Other textarea */}
        {isOtherSelected && !submitted && (
          <StyledTextarea
            className="mt-1 ml-4 "
            rows={1}
            placeholder={t("askQuestionCard.placeholder")}
            value={otherText}
            onChange={(e) => setOtherText(e.target.value)}
            autoFocus
          />
        )}
      </div>

      {/* Submit button */}
      <div className="ml-5 mt-1.5 flex items-center gap-2">
        <button
          disabled={!canSubmit}
          onClick={handleSubmit}
          className="rounded px-3 py-0.5 text-xs font-medium text-white transition-opacity hover:opacity-90 disabled:opacity-40 disabled:cursor-not-allowed"
          style={{ backgroundColor: "var(--color-accent-strong)" }}
        >
          {submitted ? t("askQuestionCard.submitted") : t("askQuestionCard.submit")}
        </button>
        {submitted && (
          <span className="text-10 text-text-tertiary ">
            Answer sent
          </span>
        )}
      </div>
    </div>
  );
}
