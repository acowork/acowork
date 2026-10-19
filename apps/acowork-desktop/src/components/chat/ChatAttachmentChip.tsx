/**
 * Chat-session attachment chip: images render as a thumbnail (served from
 * the upload-time `?thumb=1` sidecar, falling back to the full blob),
 * documents as a filename chip — the same grammar the execution composer
 * uses for its pending chips. Shared by the composer tray and the message
 * rows so a pasted screenshot never degrades to a bare filename mid-flow.
 */
import { useEffect, useState } from "react";
import { chatAttachmentObjectUrl } from "../../stores/chatSessionStore";

interface ChatAttachmentChipProps {
  chatSessionId: string;
  attachment: { id: string; filename: string; mime: string };
  /** Composer tray only — rendered as an × button when provided. */
  onRemove?: () => void;
  removeLabel?: string;
}

export function ChatAttachmentChip({
  chatSessionId,
  attachment,
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
    return (
      <span className="inline-flex items-center gap-1 rounded bg-panel-inset px-2 py-0.5 text-xs text-text-secondary">
        <span className="max-w-[12rem] truncate" title={attachment.filename}>
          {attachment.filename}
        </span>
        {removeButton}
      </span>
    );
  }

  return (
    <span className="relative inline-block h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700 align-top">
      <img src={url} alt={attachment.filename} className="h-full w-full object-cover" />
      {onRemove && (
        <button
          type="button"
          onClick={onRemove}
          aria-label={removeLabel}
          className="absolute right-0.5 top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-black/50 text-[10px] leading-none text-white transition-colors hover:bg-black/70"
        >
          ×
        </button>
      )}
    </span>
  );
}
