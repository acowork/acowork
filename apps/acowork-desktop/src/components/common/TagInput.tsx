//! ADR-104: the one chip input the app uses for free-form, validated tokens.
//!
//! ## Why a component and not three inputs
//!
//! Labels (`manifest.labels`) are the first field that is a *set of validated
//! strings* rather than a scalar, and the Create wizard is the first surface
//! that edits one. Writing the add/remove/backspace/keyboard logic inline
//! would put the "space commits a chip" contract in a dialog, where the next
//! surface (agent settings → labels, package detail → labels) would have to
//! re-derive it — and would disagree on whether a trailing comma leaves a
//! half-typed chip.
//!
//! So the *interaction* lives here and the *policy* stays with the caller:
//! this component never decides what a valid label is. It reports every raw
//! commit attempt through `onAdd`, and the caller answers by changing `value`
//! (or not). That split is what lets the same component host a strict field
//! (`labels`: lowercase kebab, mirrors `manifest::is_valid_label`) and a loose
//! one later without a second copy of the keystroke handling.

import { useRef, useState } from "react";
import { X as XIcon } from "lucide-react";
import { cn } from "../../lib/utils";

interface TagInputProps {
  /** Committed chips, in display order. */
  value: string[];
  /**
   * Called with the raw text the user committed (Enter / comma / blur).
   *
   * May be invoked with a token the caller rejects — the caller decides
   * whether to append it. Returning nothing is the "rejected" case; the
   * component keeps the input text so the user can fix it.
   */
  onAdd: (raw: string) => void;
  /** Called with the index (not the text) so duplicates stay removable. */
  onRemove: (index: number) => void;
  /** Placeholder for the empty input. */
  placeholder?: string;
  /** Disables both adding and removing (mid-submit). */
  disabled?: boolean;
  /** Marks the whole control invalid (caller owns the validation rules). */
  invalid?: boolean;
  /** Accessible name for the chip-remove buttons. */
  removeLabel?: string;
  /** Accessible name for the input itself. */
  ariaLabel?: string;
  className?: string;
}

/**
 * Chip input: type, commit with Enter or comma, remove with the chip's × or
 * Backspace-on-empty.
 *
 * Deliberately *uncontrolled about policy*: `onAdd` gets the raw token and the
 * caller mutates `value`. A rejected token keeps the text in the input (the
 * caller simply does not add it), which is what makes "invalid label, here is
 * why" possible without this component knowing the rule.
 */
export function TagInput({
  value,
  onAdd,
  onRemove,
  placeholder,
  disabled = false,
  invalid = false,
  removeLabel,
  ariaLabel,
  className,
}: TagInputProps) {
  const [text, setText] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);

  const commit = () => {
    const raw = text.trim();
    if (!raw) return;
    onAdd(raw);
    // Clear unconditionally: the caller shows a rejection message, it does
    // not hand the token back. Keeping the text would also make the *next*
    // char land after the rejected token, which reads as "it was accepted".
    setText("");
  };

  return (
    <div
      className={cn(
        "flex min-h-[var(--ui-input-py)] w-full flex-wrap items-center gap-1 rounded-md border bg-input-bg px-2 py-[calc(var(--ui-input-py)/2)]",
        invalid ? "border-red-400 dark:border-red-500" : "border-input-border",
        disabled && "opacity-60",
        className,
      )}
      onClick={() => inputRef.current?.focus()}
    >
      {value.map((chip, index) => (
        <span
          key={`${chip}-${index}`}
          className="inline-flex items-center gap-1 rounded bg-zinc-100 px-1.5 py-0.5 text-10 font-medium text-text-secondary dark:bg-zinc-700"
        >
          {chip}
          <button
            type="button"
            aria-label={removeLabel ? `${removeLabel} ${chip}` : `Remove ${chip}`}
            disabled={disabled}
            onClick={(e) => {
              // The wrapper's onClick refocuses the input; without this the
              // remove click would also steal focus *before* the chip is gone
              // and the caret would land in the removed chip's slot.
              e.stopPropagation();
              onRemove(index);
            }}
            className="text-text-tertiary hover:text-text disabled:cursor-not-allowed"
          >
            <XIcon className="h-2.5 w-2.5" />
          </button>
        </span>
      ))}
      <input
        ref={inputRef}
        type="text"
        value={text}
        disabled={disabled}
        aria-label={ariaLabel}
        aria-invalid={invalid || undefined}
        placeholder={value.length === 0 ? placeholder : undefined}
        onChange={(e) => {
          const next = e.target.value;
          // Comma commits: pasting "a, b, c" then typing on must not leave
          // one chip containing commas, which no validator would ever accept.
          if (next.includes(",") || next.includes("，")) {
            const parts = next.split(/[,，]/);
            const tail = parts.pop() ?? "";
            for (const part of parts) {
              const token = part.trim();
              if (token) onAdd(token);
            }
            setText(tail);
            return;
          }
          setText(next);
        }}
        onKeyDown={(e) => {
          if (e.key === "Enter") {
            // Enter must not submit a surrounding form / advance the wizard.
            e.preventDefault();
            commit();
            return;
          }
          if (e.key === "Backspace" && text === "" && value.length > 0) {
            onRemove(value.length - 1);
          }
        }}
        onBlur={commit}
        className="min-w-[6rem] flex-1 border-0 bg-transparent p-0 text-xs text-text outline-none placeholder:text-text-tertiary"
      />
    </div>
  );
}