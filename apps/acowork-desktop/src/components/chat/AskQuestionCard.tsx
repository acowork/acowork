import { useState } from "react";
import { MessageCircleQuestion } from "lucide-react";
import type { AskQuestionEvent } from "../../lib/types";
import { useTranslation } from "../../i18n/useTranslation";
import { useChatStore } from "../../stores/chatStore";
import { InteractionCardShell } from "./InteractionCardShell";
import { QuestionAnswerForm } from "./QuestionAnswerForm";
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
 * The chrome is `InteractionCardShell` + `QuestionAnswerForm` — the same
 * components the Chat execution block wears for the same question, so one
 * question never changes costume between the execution surface and the
 * transcript. Layout strategy matches agent messages (ml-12 +
 * content-max-width clamp).
 */
export function AskQuestionCard({ event, agentId, sessionId, onAnswer }: AskQuestionCardProps) {
  const { t } = useTranslation();
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

  const countdownLabel = remainingSecs !== null && remainingSecs > 0
    ? `${Math.floor(remainingSecs / 60)}:${String(remainingSecs % 60).padStart(2, "0")}`
    : remainingSecs === 0 ? "expired" : null;

  const handleSubmit = (answer: string) => {
    setSubmitted(true);
    onAnswer(event.request_id, answer);
  };

  return (
    <InteractionCardShell
      icon={MessageCircleQuestion}
      title={t("chatView.cardAsk")}
      className="ml-12 my-1.5 max-w-[var(--content-max-width)] w-fit"
      headerRight={
        countdownLabel ? (
          countdownLabel !== "expired" ? (
            <span
              className="text-10 font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right"
              aria-label={t("askQuestionCard.expiresIn")}
            >
              {countdownLabel}
            </span>
          ) : (
            <span className="text-10 font-mono text-text-tertiary shrink-0">
              {t("askQuestionCard.expired")}
            </span>
          )
        ) : undefined
      }
    >
      {event.title && (
        <div className="mb-1 text-xs font-medium text-text-secondary">{event.title}</div>
      )}
      {/* Same renderer on every surface, so one question reads identically
          in the execution card and in the chat block. */}
      <div className="mb-2">
        <ChatMarkdown content={event.question} />
      </div>

      <QuestionAnswerForm
        name={event.request_id}
        options={event.options ?? []}
        disabled={submitted}
        onSubmit={handleSubmit}
        submitLabel={submitted ? t("askQuestionCard.submitted") : undefined}
        note={
          submitted ? (
            <span className="text-10 text-text-tertiary">Answer sent</span>
          ) : undefined
        }
      />
    </InteractionCardShell>
  );
}
