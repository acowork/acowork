import React from "react";
import { cn } from "../../lib/utils";

/**
 * The field surface: border, fill, padding and focus treatment every form
 * control in the app shares.
 *
 * Exported because a control is not always an `<input>` — `DatePicker`'s
 * trigger is a `<button>` opening a calendar, and it has to sit in the same
 * row as the text fields without looking like a different family. Copying the
 * class string into it would be a second source of truth that drifts the first
 * time this surface changes; composing `FIELD_SURFACE_CLASS` cannot drift.
 */
export const FIELD_SURFACE_CLASS =
  "w-full rounded-md border border-input-border bg-input-bg px-3 py-[var(--ui-input-py)] text-xs outline-none transition-colors focus:border-[var(--color-accent)]";

/**
 * Unified input field component with consistent focus style:
 * thin accent-colored border on focus (no ring / outline).
 *
 * Note: width comes from the caller's `className` (defaults to `w-full`).
 * We deliberately run the merge via `cn()` / `tailwind-merge` so a caller
 * passing `w-16` / `w-24` / `w-32` etc. correctly overrides the default
 * `w-full` — plain string concat would let `w-full` win via CSS cascade
 * order, leaving narrow inputs stretched across the whole row (see
 * Settings > Logs: "Log File Size (MB)" / "Max Log Files").
 */
interface StyledInputProps extends React.InputHTMLAttributes<HTMLInputElement> {
  /** Use monospace font (for API keys, code, etc.) */
  fontMono?: boolean;
}

export const StyledInput = React.forwardRef<HTMLInputElement, StyledInputProps>(
  ({ fontMono, className = "", ...props }, ref) => {
    return (
      <input
        ref={ref}
        className={cn(
          FIELD_SURFACE_CLASS,
          fontMono && "font-mono",
          className,
        )}
        {...props}
      />
    );
  },
);
StyledInput.displayName = "StyledInput";

/**
 * Unified textarea with consistent focus style.
 *
 * Same width-merging rationale as `StyledInput`: `w-full` is the default
 * but must be overridable by the caller's `className`.
 */
interface StyledTextareaProps
  extends React.TextareaHTMLAttributes<HTMLTextAreaElement> {
  /** Use monospace font */
  fontMono?: boolean;
}

export const StyledTextarea = React.forwardRef<
  HTMLTextAreaElement,
  StyledTextareaProps
>(({ fontMono, className = "", ...props }, ref) => {
  return (
    <textarea
      ref={ref}
      className={cn(
        FIELD_SURFACE_CLASS,
        "resize-y",
        fontMono && "font-mono",
        className,
      )}
      {...props}
    />
  );
});
StyledTextarea.displayName = "StyledTextarea";
