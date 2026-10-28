/**
 * Chat-session attachment chip: images render as a thumbnail (served from
 * the upload-time `?thumb=1` sidecar, falling back to the full blob),
 * documents as a filename capsule.
 *
 * The frame comes from [`AttachmentCapsule`] — the same primitive the
 * execution surface's workspace refs use — so a chat attachment no longer
 * reads as a grey caption next to a proper capsule. Shared by the composer
 * tray and the message rows so a pasted screenshot never degrades to a bare
 * filename mid-flow.
 */
import { useEffect, useState } from "react";
import { FileText, X } from "lucide-react";
import { chatAttachmentObjectUrl } from "../../stores/chatSessionStore";
import type { ChatAttachment } from "../../lib/types";
import { AttachmentCapsule, formatAttachmentSize } from "./AttachmentCapsule";

/**
 * The minimum a chip needs to render, plus the two fields that only change
 * what a *click* can do. Rows carry the full `ChatAttachment`; older call
 * sites (and tests) pass the three fields alone, and the chip must stay a
 * plain capsule rather than promise a size it does not have.
 */
export type ChatAttachmentChipInfo = Pick<ChatAttachment, "id" | "filename" | "mime"> &
  Partial<Pick<ChatAttachment, "size" | "thumb">>;

interface ChatAttachmentChipProps {
  chatSessionId: string;
  attachment: ChatAttachmentChipInfo;
  /** Message rows: opens the file. Absent = decorative (composer tray). */
  onClick?: () => void;
  /** Composer tray only — rendered as an × button when provided. */
  onRemove?: () => void;
  removeLabel?: string;
}

export function ChatAttachmentChip({
  chatSessionId,
  attachment,
  onClick,
  onRemove,
  removeLabel,
}: ChatAttachmentChipProps) {
  const isImage = attachment.mime.startsWith("image/");
  const [url, setUrl] = useState<string | null>(null);

  useEffect(() => {
    if (!isImage) return;
    let alive = true;
    void (async () => {
      try {
        try {
          return await chatAttachmentObjectUrl(chatSessionId, attachment.id, { thumb: true });
        } catch {
          // No sidecar (decoder rejected the format) — the full blob is
          // still a valid preview; it just costs more.
          return await chatAttachmentObjectUrl(chatSessionId, attachment.id);
        }
      } catch {
        // Both fetches failed (deleted blob, offline) — stay a filename chip.
        return null;
      }
    })().then((u) => {
      if (alive && u) setUrl(u);
    });
    return () => {
      alive = false;
    };
  }, [isImage, chatSessionId, attachment.id]);

  const removeButton = onRemove && (
    <button
      type="button"
      onClick={onRemove}
      aria-label={removeLabel}
      className="text-text-tertiary transition-colors hover:text-text-primary"
    >
      ×
    </button>
  );

  if (!isImage || !url) {
    const size = typeof attachment.size === "number" ? attachment.size : null;
    // One interactive element per chip: a row opens its file, the composer
    // tray's × removes it, and a caller that passes both gets a decorative
    // capsule holding the × rather than a `<button>` nested in a `<button>`
    // (invalid HTML, and two tab stops for one control). Same rule as the
    // thumbnail branch below.
    return (
      <AttachmentCapsule
        onClick={onRemove ? undefined : onClick}
        title={
          size === null
            ? `${attachment.filename} (${attachment.mime})`
            : `${attachment.filename} (${attachment.mime}, ${formatAttachmentSize(size)})`
        }
        ariaLabel={attachment.filename}
        trailing={removeButton}
      >
        <FileText className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        <span className="max-w-[12rem] truncate font-medium">{attachment.filename}</span>
        {size !== null && (
          <span className="shrink-0 text-text-tertiary">{formatAttachmentSize(size)}</span>
        )}
      </AttachmentCapsule>
    );
  }

  // A thumbnail is a picture, not a capsule: it keeps its 56×56 box. It does
  // gain the click affordance the capsule has, so "files you can open look
  // openable" holds for the image branch too. The `!onRemove` half is the same
  // single-interactive-element rule as the capsule branch above: the remove
  // button owns the click when it exists.
  const interactive = !!onClick && !onRemove;
  const Tag = interactive ? "button" : "div";
  return (
    <Tag
      type={interactive ? "button" : undefined}
      onClick={interactive ? onClick : undefined}
      aria-label={interactive ? attachment.filename : undefined}
      title={attachment.filename}
      className={
        "relative inline-block h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 align-top dark:border-zinc-700"
        + (interactive
          ? " cursor-pointer transition-shadow hover:ring-2 hover:ring-[var(--color-accent)]/50"
          : "")
      }
    >
      <img src={url} alt={attachment.filename} className="h-full w-full object-cover" />
      {onRemove && (
        <button
          type="button"
          onClick={onRemove}
          aria-label={removeLabel}
          className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-black/50 text-white transition-colors hover:bg-black/70"
        >
          <X size={8} />
        </button>
      )}
    </Tag>
  );
}