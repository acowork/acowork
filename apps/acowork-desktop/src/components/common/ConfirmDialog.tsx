import { useEffect, useRef } from "react";
import { cn } from "../../lib/utils";
import { useEscapeClose } from "../../hooks/useEscapeClose";

interface ConfirmDialogProps {
  open: boolean;
  title: string;
  message: string;
  confirmLabel?: string;
  destructive?: boolean;
  onConfirm: () => void;
  onCancel: () => void;
}

export function ConfirmDialog({
  open,
  title,
  message,
  confirmLabel = "Confirm",
  destructive: _destructive = false, // accepted for API back-compat; currently both branches use btn-accent
  onConfirm,
  onCancel,
}: ConfirmDialogProps) {
  const cancelRef = useRef<HTMLButtonElement>(null);

  // Focus cancel on open — keyed on `open` alone. `AgentList` passes an
  // inline `onCancel`, so listing it here re-ran `focus()` on every parent
  // re-render and stole focus from whatever the user was using.
  useEffect(() => {
    if (!open) return;
    cancelRef.current?.focus();
  }, [open]);

  // Close on Escape
  useEscapeClose(open, onCancel);

  if (!open) return null;

  return (
    <div className="fixed inset-0 z-50 flex items-center justify-center">
      {/* Backdrop */}
      <div className="absolute inset-0 bg-modal-overlay" onClick={onCancel} />

      {/* Dialog — standard 3-row layout: header / body / footer, two
          dividers, matching PublishWizard / CloneDialog / CreateWizard. */}
      <div
        className="relative z-10 flex w-full max-w-sm flex-col rounded-md border border-border-outer bg-modal-surface shadow-xl"
        role="alertdialog"
        aria-modal="true"
        aria-labelledby="confirm-title"
        aria-describedby="confirm-desc"
      >
        {/* Header */}
        <h3
          id="confirm-title"
          className="flex items-center border-b border-border-divider px-5 py-3 min-h-[var(--ui-dialog-zone-h)] text-sm font-semibold text-text"
        >
          {title}
        </h3>

        {/* Body */}
        <p
          id="confirm-desc"
          className="px-5 py-4 text-xs text-text-tertiary"
        >
          {message}
        </p>

        {/* Footer */}
        <div className="flex items-center justify-end gap-2 border-t border-border-divider px-5 min-h-[var(--ui-dialog-zone-h)]">
          <button
            ref={cancelRef}
            onClick={onCancel}
            className="rounded-md px-3 py-1.5 text-xs font-medium text-text-secondary hover:bg-[var(--color-accent)]/10  dark:hover:bg-[var(--color-accent)]/15"
          >
            Cancel
          </button>
          <button
            onClick={onConfirm}
            className={cn(
              "rounded-md px-3 py-1.5 text-xs font-medium",
              "btn-accent",
            )}
          >
            {confirmLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
