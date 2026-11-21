/**
 * ADR-119: the turn's deliverables row — one line of thumbnails and file chips
 * under the last assistant bubble.
 *
 * ## Why it sits here and not on the tool block
 *
 * A real session renders a dozen `Explored (N steps)` folds per screen,
 * interleaved with assistant text, and those folds are collapsed by default.
 * Any per-step surface is therefore either invisible (inside the fold) or
 * noise (a badge on every fold). One row **per turn** is the only granularity
 * that does not scale with the step count: the user reviews the turn's result,
 * not the hundred steps that produced it.
 *
 * The row is a persisted `system` entry, so it survives replay: reopening the
 * session shows the same thumbnails, because they describe files that are
 * still on the node.
 *
 * ## Why a click opens the editor's viewer
 *
 * A deliverable is `(workspace_id, rel_path)` — the same coordinates the
 * workspace panel uses. Clicking one calls `openFile`, exactly as clicking the
 * file in the tree does, so the image lands in the *existing* editor preview
 * tab (zoom / pan / region-to-chat) and there is no second viewer to maintain.
 *
 * Bytes are re-fetched through the ordinary workspace file route and cached by
 * the same LRU; nothing about a deliverable travels over the message channel.
 */
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { FileText, Loader2, PackageOpen } from "lucide-react";
import type { ChatMessage } from "../../lib/types";
import {
  deliverableFileName,
  formatDeliverableSize,
  isImageDeliverable,
  parseDeliverableFiles,
  type DeliverableFile,
} from "../../lib/deliverableFiles";
import { fetchWorkspaceFileBlobUrl } from "../../lib/workspaceFileBlob";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useTranslation } from "../../i18n/useTranslation";
import { log } from "../../lib/logger";
import { cn } from "../../lib/utils";

export interface ChatDeliverablesRowProps {
  /** The row as loaded from the transcript (`metadata.type === "deliverables"`). */
  message: ChatMessage;
  /** Agent whose node holds the bytes. Without it nothing is fetchable. */
  agentId?: string;
}

export function ChatDeliverablesRow({ message, agentId }: ChatDeliverablesRowProps) {
  const { t } = useTranslation();
  const files = useMemo(() => parseDeliverableFiles(message.metadata), [message.metadata]);

  const openFile = useCallback(
    (file: DeliverableFile) => {
      if (!agentId) return;
      void useFileEditorStore
        .getState()
        .openFile(agentId, file.workspaceId, file.relPath);
    },
    [agentId],
  );

  if (files.length === 0 || !agentId) return null;

  return (
    <div className="mb-2 ml-12 mt-1" data-deliverables-row="true">
      <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
        <PackageOpen className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
        <span>
          {files.length === 1
            ? t("deliverables.title_one")
            : t("deliverables.title", { count: files.length })}
        </span>
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-2">
        {files.map((file) =>
          isImageDeliverable(file) ? (
            <DeliverableThumbnail
              key={`${file.workspaceId}:${file.relPath}`}
              file={file}
              agentId={agentId}
              onOpen={openFile}
            />
          ) : (
            <DeliverableChip
              key={`${file.workspaceId}:${file.relPath}`}
              file={file}
              onOpen={openFile}
            />
          ),
        )}
      </div>
    </div>
  );
}

/** A 56×56 preview of an image deliverable. */
function DeliverableThumbnail({
  file,
  agentId,
  onOpen,
}: {
  file: DeliverableFile;
  agentId: string;
  onOpen: (file: DeliverableFile) => void;
}) {
  const { t } = useTranslation();
  const [url, setUrl] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  /** Set once the fetch resolves: the URL belongs to the blob LRU, which
   *  revokes it on eviction, so this component must NOT revoke it itself. */
  const aliveRef = useRef(true);

  useEffect(() => {
    aliveRef.current = true;
    const ctrl = new AbortController();
    void fetchWorkspaceFileBlobUrl(agentId, file.workspaceId, file.relPath, {
      signal: ctrl.signal,
    })
      .then((result) => {
        if (!aliveRef.current) return;
        setUrl(result.blobUrl);
      })
      .catch((err) => {
        if (!aliveRef.current) return;
        log.warn("[ChatDeliverablesRow] thumbnail fetch failed:", err);
        setFailed(true);
      });
    return () => {
      aliveRef.current = false;
      ctrl.abort();
    };
  }, [agentId, file.workspaceId, file.relPath]);

  const name = deliverableFileName(file);

  // A thumbnail we cannot load is still a file the user can open: fall back to
  // the chip rather than leaving a broken-image hole.
  if (failed) {
    return (
      <DeliverableChip
        file={file}
        onOpen={onOpen}
        title={t("deliverables.openFailed", { filename: name })}
      />
    );
  }

  return (
    <button
      type="button"
      onClick={() => onOpen(file)}
      title={`${name} · ${formatDeliverableSize(file.bytes)}`}
      aria-label={t("deliverables.imageAlt", { filename: name })}
      className={cn(
        "group relative h-14 w-14 shrink-0 cursor-zoom-in overflow-hidden rounded-md",
        "border border-border-outer bg-surface",
      )}
    >
      {url ? (
        <img src={url} alt={name} className="h-full w-full object-cover" />
      ) : (
        <span className="flex h-full w-full items-center justify-center">
          <Loader2 className="h-4 w-4 animate-spin text-text-tertiary" aria-hidden="true" />
        </span>
      )}
    </button>
  );
}

/** A capsule for a non-image deliverable (a PDF, a report, a data file). */
function DeliverableChip({
  file,
  onOpen,
  title,
}: {
  file: DeliverableFile;
  onOpen: (file: DeliverableFile) => void;
  title?: string;
}) {
  const name = deliverableFileName(file);
  return (
    <button
      type="button"
      onClick={() => onOpen(file)}
      title={title ?? `${name} · ${formatDeliverableSize(file.bytes)}`}
      className={cn(
        "flex max-w-[16rem] items-center gap-1.5 rounded-md border border-border-outer",
        "bg-surface px-2 py-1.5 text-xs text-text-secondary",
        "hover:border-[var(--color-accent)]/40 hover:bg-surface-hover",
      )}
    >
      <FileText className="h-3.5 w-3.5 shrink-0 text-text-tertiary" aria-hidden="true" />
      <span className="truncate">{name}</span>
      <span className="shrink-0 text-text-tertiary">
        {formatDeliverableSize(file.bytes)}
      </span>
    </button>
  );
}