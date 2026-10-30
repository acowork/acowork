/**
 * The attachment capsule — one visual grammar for every "a file hangs off a
 * message" affordance on both chat-like surfaces.
 *
 * Why this exists: the chat surface (`ChatAttachmentChip`) and the execution
 * surface (`AttachmentChipRow`) each hand-rolled the same chip, so they drifted
 * — chat attachments ended up as a borderless grey `span` that read like a
 * caption, while the execution session's workspace refs were proper capsules
 * with a border, an icon and a hover state. Two surfaces, two looks, one
 * meaning. This component owns the box; callers own the content (icon, label,
 * trailing button), so the frame cannot drift again.
 *
 * `Tag` is chosen by intent, not by style: a clickable capsule is a real
 * `button` (keyboard reachable, no `role` hack), a non-interactive one is a
 * `span`. That is the only behavioural difference between the two forms.
 */
import type { ReactNode } from "react";
import { cn } from "../../lib/utils";

export interface AttachmentCapsuleProps {
  /** Makes the capsule interactive. Absent = decorative `span`. */
  onClick?: () => void;
  /** Tooltip. Callers pass the file's provenance (path, mime, size…). */
  title?: string;
  /** Tight padding for dense rows and composer trays. */
  compact?: boolean;
  /** Dim + non-interactive while an optimistic row awaits confirmation. */
  pending?: boolean;
  /** Accessible name for the `button` form; ignored on the `span` form. */
  ariaLabel?: string;
  className?: string;
  /** Trailing control (e.g. the composer's remove button). */
  trailing?: ReactNode;
  "data-testid"?: string;
  children: ReactNode;
}

export function AttachmentCapsule({
  onClick,
  title,
  compact = true,
  pending = false,
  ariaLabel,
  className,
  trailing,
  "data-testid": testId,
  children,
}: AttachmentCapsuleProps) {
  const interactive = !!onClick && !pending;
  const classes = cn(
    "inline-flex items-center gap-1.5 rounded-md border border-border-outer",
    "bg-zinc-50 text-xs text-text-secondary dark:bg-zinc-800",
    compact ? "px-2.5 py-1" : "px-3 py-1.5",
    interactive && "cursor-pointer hover:bg-zinc-100 dark:hover:bg-zinc-700",
    pending && "opacity-60",
    className,
  );

  if (!interactive) {
    return (
      <span className={classes} title={title} data-testid={testId}>
        {children}
        {trailing}
      </span>
    );
  }

  return (
    <button
      type="button"
      className={classes}
      title={title}
      aria-label={ariaLabel ?? title}
      onClick={onClick}
      data-testid={testId}
    >
      {children}
      {trailing}
    </button>
  );
}

/**
 * `900 B` / `12.0 KB` / `3.4 MB` — one decimal above 1 KB, matching the
 * execution surface's `DocumentChip` so the two cannot disagree about what
 * the same file weighs.
 *
 * Deliberately the desktop rendering and not the router's: the Chat Router and
 * mobile both write `12KB` for the same file (`chat_router::context::human_size`,
 * `mobile/lib/chatAttachment.humanSize`), because that line has to stay short.
 * Two spellings of one size is a cosmetic split, not a bug — but it is why this
 * is a local helper rather than a shared one, and why `user-chat-api.ts`'s
 * older `formatAttachmentSize` (which rounds to `13 KB`) must not be reused
 * here.
 */
export function formatAttachmentSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}