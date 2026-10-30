/**
 * ADR-046 §2.5: Unified attachment chip row — renders a single attachment
 * entry as a styled chip / thumbnail widget. Dispatched by `kind`:
 *
 *   - `file_upload`      → DocumentChip (legacy component, unchanged).
 *   - `image_upload`     → 56×56 thumbnail. Lazily fetches the blob from
 *                          `GET /api/agents/{agentId}/files/{documentId}`
 *                          via `URL.createObjectURL`; revokes on unmount.
 *                          Uses `width`/`height` from metadata as CSS
 *                          aspect-ratio hint; falls back to `<img onLoad>`
 *                          natural sizing when absent.
 *   - `attached_file`    → workspace ref chip.
 *   - `attached_selection` → workspace ref chip + line range.
 *   - `attached_folder`  → workspace ref chip.
 *
 * The three workspace arms render through ONE `WorkspaceRefChip`; which glyph,
 * tint and range it shows is read from `attachmentGlyph` and the item itself,
 * so a new arm is a row in that table and not a new component.
 *
 * Each attachment is a stand-alone system entry in the message list (NOT
 * inline in a user bubble). Clickable workspace refs call `onChipClick`.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Loader2, X } from "lucide-react";
import type { AttachedFileItem, AttachedFolderItem, AttachedItem, AttachedSelectionItem, ImageUploadItem } from "../../lib/types";
import { getGatewayUrl } from "../../lib/config";
import { log } from "../../lib/logger";
import { cn } from "../../lib/utils";
import { attachmentGlyph, selectionRange } from "./attachmentGlyph";
import { AttachmentCapsule } from "./AttachmentCapsule";
import { DocumentChip } from "./DocumentChip";

export interface AttachmentChipRowProps {
  item: AttachedItem;
  /** Required for `image_upload` thumbnail fetch. Optional — other variants
   *  silently ignore it. */
  agentId?: string | null;
  /** Tighten layout for compact display (default `true`). */
  compact?: boolean;
  /** Called when the user clicks on a workspace-ref chip (attached_file,
   *  attached_selection, attached_folder). File uploads call this with
   *  `null` — they are not clickable. */
  onChipClick?: (item: AttachedItem) => void;
  /** Called when the user clicks the remove button (upload chips only). */
  onRemove?: (item: AttachedItem) => void;
  /** When true, renders the chip in a pending (optimistic) state with
   *  reduced opacity and a spinner icon, indicating the server has not
   *  yet confirmed the attachment entry. */
  pending?: boolean;
}

export function AttachmentChipRow({
  item,
  agentId,
  compact = true,
  onChipClick,
  onRemove,
  pending = false,
}: AttachmentChipRowProps) {
  switch (item.type) {
    case "file_upload":
      return (
        <DocumentChip
          filename={item.filename}
          format={item.format}
          size={item.sizeBytes}
          status="success"
          onRemove={onRemove ? () => onRemove(item) : undefined}
        />
      );
    case "image_upload":
      return (
        <ImageAttachmentThumbnail
          item={item}
          agentId={agentId ?? null}
          onRemove={onRemove ? () => onRemove(item) : undefined}
          pending={pending}
        />
      );
    case "attached_file":
    case "attached_selection":
    case "attached_folder":
      // One component for the three workspace arms: they are the same chip
      // with a different glyph and an optional line range, both of which it
      // reads off the item itself.
      return (
        <WorkspaceRefChip
          item={item}
          compact={compact}
          onClick={onChipClick ? () => onChipClick(item) : undefined}
          pending={pending}
        />
      );
  }
}

// ── Sub-components ─────────────────────────────────────────────────

/**
 * The three workspace arms are one chip.
 *
 * They differed only in glyph, tint and whether a line range is printed — all
 * three of which the glyph table and the item's own fields already say. Keeping
 * three copies meant three edits per change and three chances to drift, which is
 * the fragmentation this file's capsule + table exist to remove.
 */
function WorkspaceRefChip({
  item,
  compact,
  onClick,
  pending,
}: {
  item: AttachedFileItem | AttachedSelectionItem | AttachedFolderItem;
  compact: boolean;
  onClick?: () => void;
  pending?: boolean;
}) {
  const { Icon, className } = attachmentGlyph(item.type);
  const range =
    item.type === "attached_selection"
      ? selectionRange(item.startLine, item.endLine)
      : undefined;
  const provenance = range ? `${item.absPath} ${range}` : item.absPath;
  return (
    <AttachmentCapsule
      compact={compact}
      pending={pending}
      title={pending ? "等待服务器确认..." : provenance}
      onClick={onClick}
    >
      {pending ? (
        <Loader2 className={cn("h-4 w-4 shrink-0 animate-spin", className)} />
      ) : (
        <Icon className={cn("h-4 w-4 shrink-0", className)} />
      )}
      <span className="max-w-[200px] truncate font-medium">{item.name}</span>
      {range && <span className="text-text-tertiary">{range}</span>}
    </AttachmentCapsule>
  );
}

/**
 * Thumbnail for `image_upload` items. Fetches the blob on mount, revokes the
 * resulting ObjectURL on unmount.
 *
 * ADR-046 §7: `width`/`height` from metadata are used as CSS aspect-ratio
 * hint when present. When absent, the `<img>` element relies on `onLoad` to
 * read the natural size as a fallback.
 */
function ImageAttachmentThumbnail({
  item,
  agentId,
  onRemove,
  pending,
}: {
  item: ImageUploadItem;
  agentId: string | null;
  onRemove?: () => void;
  pending?: boolean;
}) {
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  const [naturalLoaded, setNaturalLoaded] = useState(false);
  const imgRef = useRef<HTMLImageElement>(null);
  /** The row's own object URL, released from the cleanup body. It cannot be
   *  released through a `setState` updater: React drops updates to an
   *  unmounted component, so the updater never runs — which is exactly when
   *  the release matters. A transcript row is unmounted the moment it scrolls
   *  out of the virtualized window, so the previous spelling leaked one object
   *  URL per image the user ever scrolled past. */
  const urlRef = useRef<string | null>(null);

  useEffect(() => {
    if (!agentId) return;
    let cancelled = false;
    const ctrl = new AbortController();
    const blobUrl = `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/files/${encodeURIComponent(item.documentId)}`;
    fetch(blobUrl, { signal: ctrl.signal })
      .then((resp) => {
        if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
        return resp.blob();
      })
      .then((blob) => {
        if (cancelled) return;
        const objectUrl = URL.createObjectURL(blob);
        urlRef.current = objectUrl;
        setUrl(objectUrl);
      })
      .catch((err) => {
        if (cancelled || (err instanceof DOMException && err.name === "AbortError")) {
          return;
        }
        log.warn("[AttachmentChipRow] image blob fetch failed:", err);
        setFailed(true);
      });
    return () => {
      cancelled = true;
      ctrl.abort();
      if (urlRef.current) {
        URL.revokeObjectURL(urlRef.current);
        urlRef.current = null;
      }
      setUrl(null);
    };
  }, [agentId, item.documentId]);

  const handleLoad = useCallback(() => {
    setNaturalLoaded(true);
    // When width/height are absent from metadata, use natural size as
    // fallback (ADR-046 §7: both paths are tolerated).
    if (!item.width || !item.height) {
      const img = imgRef.current;
      if (img && img.naturalWidth && img.naturalHeight) {
        img.style.aspectRatio = `${img.naturalWidth} / ${img.naturalHeight}`;
      }
    }
  }, [item.width, item.height]);

  // Use metadata width/height as aspect-ratio hint when present.
  // When absent, the img's natural aspect ratio takes over via onLoad.
  const aspectRatio =
    item.width && item.height
      ? `${item.width} / ${item.height}`
      : undefined;

  return (
    <div
      className={"group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 bg-zinc-100 dark:border-zinc-700 dark:bg-zinc-800" + (pending ? " opacity-60" : "")}
      title={pending ? "等待服务器确认..." : `${item.filename} (${item.format})`}
      style={aspectRatio ? { aspectRatio } : undefined}
    >
      {url ? (
        <img
          ref={imgRef}
          src={url}
          alt={item.filename}
          className="h-full w-full object-cover"
          onLoad={handleLoad}
          style={
            !naturalLoaded && aspectRatio
              ? { opacity: 0.5 }
              : undefined
          }
        />
      ) : (
        <div className="flex h-full w-full items-center justify-center text-10 text-text-tertiary">
          {failed ? "!" : "…"}
        </div>
      )}
      {onRemove && (
        <button
          type="button"
          onClick={onRemove}
          className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
          aria-label={`Remove ${item.filename}`}
        >
          <X size={10} />
        </button>
      )}
    </div>
  );
}