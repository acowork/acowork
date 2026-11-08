import { useState } from "react";
import type { ReactNode } from "react";
import type { QuestionOption } from "../../lib/types";
import { StyledTextarea } from "../common/StyledInput";
import { useTranslation } from "../../i18n/useTranslation";
import { listRowHover, listRowSelected } from "../../lib/ui-styles";

/** The sentinel that marks the free-text "Other" choice. Never a real label:
 *  the runtime's own `ask_user_question` schema rejects it as an option. */
const OTHER = "__other__";

interface QuestionAnswerFormProps {
  /** Radio-group identity; the request id is the natural one. */
  name: string;
  options: QuestionOption[];
  /** The question is no longer clickable: submitted on the execution
   *  surface, mid-delivery on the chat surface. */
  disabled?: boolean;
  onSubmit: (answer: string) => void;
  /** Primary-action copy; the execution card swaps it for "Submitted". */
  submitLabel?: string;
  /** Trailing copy next to the submit button (sent-note, failure-note). */
  note?: ReactNode;
}

/**
 * QuestionAnswerForm — the ask_question body: a radio list of the model's
 * options, an "Other" row that opens a real textarea, and one submit button.
 *
 * This is the execution surface's `AskQuestionCard` body, lifted so the same
 * question inside a Chat execution block answers in the same costume
 * (`InteractionCardShell` + this form). The two surfaces differ only in what
 * a submit does and in who owns the disabled state — both are props.
 */
export function QuestionAnswerForm({
  name,
  options,
  disabled,
  onSubmit,
  submitLabel,
  note,
}: QuestionAnswerFormProps) {
  const { t } = useTranslation();
  const [selected, setSelected] = useState<string | null>(null);
  const [otherText, setOtherText] = useState("");

  const isOtherSelected = selected === OTHER;
  const canSubmit =
    !disabled && selected !== null && (!isOtherSelected || otherText.trim().length > 0);

  const rowClass = (isSelected: boolean) =>
    `flex items-center gap-2 rounded px-2 py-1 text-xs transition-colors cursor-pointer
      ${disabled ? "opacity-60 pointer-events-none" : listRowHover}
      ${isSelected ? listRowSelected : ""}`;

  return (
    <>
      {/* Sentence-shaped options wrap; a centered label that wraps reads as
          two options. */}
      <ul className="mb-2 space-y-0.5">
        {options.map((opt, idx) => (
          <li
            key={idx}
            className={rowClass(selected === opt.label)}
            onClick={() => !disabled && setSelected(opt.label)}
          >
            <input
              type="radio"
              name={`question-${name}`}
              value={opt.label}
              checked={selected === opt.label}
              disabled={disabled}
              onChange={() => setSelected(opt.label)}
              className="shrink-0 h-3 w-3"
              style={{ accentColor: "var(--color-accent)" }}
            />
            <span className="font-medium text-text break-words">{opt.label}</span>
            {opt.description && (
              <span className="text-text-tertiary">— {opt.description}</span>
            )}
          </li>
        ))}

        {/* "Other" option */}
        <li
          className={rowClass(isOtherSelected)}
          onClick={() => !disabled && setSelected(OTHER)}
        >
          <input
            type="radio"
            name={`question-${name}`}
            value={OTHER}
            checked={isOtherSelected}
            disabled={disabled}
            onChange={() => setSelected(OTHER)}
            className="shrink-0 h-3 w-3"
            style={{ accentColor: "var(--color-accent)" }}
          />
          <span className="font-medium text-text">{t("askQuestionCard.other")}</span>
        </li>
      </ul>

      {/* The free answer is a real field, not an inline input next to a
          button — same `StyledTextarea` the rest of the app's forms use. */}
      {isOtherSelected && (
        <StyledTextarea
          className="mb-2 ml-4"
          rows={2}
          placeholder={t("askQuestionCard.placeholder")}
          value={otherText}
          onChange={(e) => setOtherText(e.target.value)}
          disabled={disabled}
          autoFocus
        />
      )}

      <div className="flex items-center gap-2">
        <button
          type="button"
          disabled={!canSubmit}
          onClick={() => {
            if (!canSubmit) return;
            onSubmit(isOtherSelected ? otherText.trim() : selected!);
          }}
          className="rounded btn-accent px-3 py-1.5 text-xs disabled:opacity-40 disabled:cursor-not-allowed"
        >
          {submitLabel ?? t("askQuestionCard.submit")}
        </button>
        {note}
      </div>
    </>
  );
}
