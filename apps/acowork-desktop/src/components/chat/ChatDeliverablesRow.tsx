/**
 * The turn's deliverables row — one line of thumbnails, file chips and online
 * document cards under the last assistant bubble.
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
 * The row is derived from the transcript's tool calls ([deriveDeliverables.ts]),
 * so it survives replay: reopening the session shows the same deliverables,
 * because they describe files / documents that still exist.
 *
 * ## Why a click opens the editor's viewer
 *
 * A workspace deliverable is `(workspace_id, rel_path)` — the same coordinates
 * the workspace panel uses. Clicking one goes through the **same chat-side
 * rule every other clickable reference in chat uses**
 * (`pickOpenActionForPath`: a format with an in-app viewer — image, Markdown,
 * HTML, PDF, Office, craft document — opens there, source and data files open
 * in Monaco),
 * so it lands in the *existing* editor tab and there is no second viewer to
 * maintain. Binary formats in particular must not take the text path: the read
 * interface returns them base64-encoded, which is what used to render as a wall
 * of characters.
 *
 * An online document is addressable by `doc_id`; clicking it goes through
 * `fileEditorStore.openDocEntry` with the shared `DocEntryRef` shape, which
 * picks the view by extension (Markdown body vs the byte viewer) — so a
 * `.pptx` deliverable opens as a deck, not as a blank editor. The row never
 * carries bytes for either kind.
 *
 * Bytes are re-fetched through the ordinary workspace file route by the same
 * hook the editor preview uses, and cached by the same LRU; nothing about a
 * deliverable travels over the message channel.
 */
import { useCallback, useMemo, useState } from "react";
import { Loader2, PackageOpen } from "lucide-react";
import { CopyButton } from "../common/CopyButton";
import type { ChatMessage } from "../../lib/types";
import {
  deliverableFileName,
  formatDeliverableSize,
  isImageDeliverable,
  parseDeliverableFiles,
  type DeliverableFile,
} from "../../lib/deliverableFiles";
import {
  deliverableDocFileName,
  type DeliverableDoc,
} from "../../lib/deliverableDocs";
import { useWorkspaceFileBlob } from "../../hooks/useWorkspaceFileBlob";
import { useFileEditorStore } from "../../stores/fileEditorStore";
// Open-mode rule lives in the leaf module (filePreview), not in the markdown
// renderer that also uses it for `<a>` interception.
import { pickOpenActionForPath } from "../editor/filePreview";
import { getFileIcon } from "../workspace/FileTree/fileIcons";
import { SetiIcon } from "../common/SetiIcon";
import { useTranslation } from "../../i18n/useTranslation";
import { cn } from "../../lib/utils";

export interface ChatDeliverablesRowProps {
  /** The row as loaded / derived (`metadata.type === "deliverables"`). */
  message: ChatMessage;
  /** Agent whose node holds the workspace bytes. Without it workspace files are
   *  not fetchable; online documents still open through the Gateway doc proxy. */
  agentId?: string;
}

/** Parse the derived / legacy document cards off the row metadata. */
function parseDocs(metadata: Record<string, unknown> | undefined): DeliverableDoc[] {
  const raw = metadata?.docs;
  if (!Array.isArray(raw)) return [];
  const out: DeliverableDoc[] = [];
  const seen = new Set<string>();
  for (const entry of raw) {
    if (!entry || typeof entry !== "object") continue;
    const r = entry as Record<string, unknown>;
    const docId = typeof r.docId === "string" ? r.docId : "";
    const path = typeof r.path === "string" ? r.path : "";
    if (!docId || !path || seen.has(docId)) continue;
    seen.add(docId);
    const name = typeof r.name === "string" && r.name ? r.name : path;
    const version = typeof r.version === "number" && Number.isFinite(r.version) ? r.version : 0;
    out.push({ docId, path, name, version });
  }
  return out;
}

export function ChatDeliverablesRow({ message, agentId }: ChatDeliverablesRowProps) {
  const { t } = useTranslation();
  const files = useMemo(() => parseDeliverableFiles(message.metadata), [message.metadata]);
  const docs = useMemo(() => parseDocs(message.metadata), [message.metadata]);

  /** Open one workspace deliverable the way every other chat-side file
   *  reference opens: viewer for a previewable format, Monaco for the rest. */
  const openEntry = useCallback(
    (file: DeliverableFile) => {
      if (!agentId) return;
      const store = useFileEditorStore.getState();
      if (pickOpenActionForPath(file.relPath) === "openPreview") {
        void store.openPreview(agentId, file.workspaceId, file.relPath);
      } else {
        void store.openFile(agentId, file.workspaceId, file.relPath);
      }
    },
    [agentId],
  );

  /** Open an online document through the one doc entry point. Which view it
   *  gets (Markdown body vs byte viewer) is `openDocEntry`'s call — the card
   *  does not know or care whether the deliverable is a `.md` or a `.pptx`. */
  const openDoc = useCallback((doc: DeliverableDoc) => {
    useFileEditorStore.getState().openDocEntry({
      docId: doc.docId,
      path: doc.path,
      name: doc.name,
      ext: null,
    });
  }, []);

  // Workspace files need an agent to fetch bytes; documents do not (the doc
  // proxy is Gateway-wide). Show the row whenever either kind is present.
  const showFiles = files.length > 0 && !!agentId;
  const total = (showFiles ? files.length : 0) + docs.length;
  if (total === 0) return null;

  return (
    <div className="mb-2 ml-12 mt-1" data-deliverables-row="true">
      <div className="flex items-center gap-1.5 text-xs text-text-tertiary">
        <PackageOpen className="h-3.5 w-3.5 shrink-0" aria-hidden="true" />
        <span>
          {total === 1
            ? t("deliverables.title_one")
            : t("deliverables.title", { count: total })}
        </span>
      </div>
      <div className="mt-1.5 flex flex-wrap items-center gap-2">
        {showFiles &&
          files.map((file) => (
            <DeliverableLinkCopy
              key={`${file.workspaceId}:${file.relPath}`}
              text={file.relPath}
            >
              {isImageDeliverable(file) ? (
                <DeliverableThumbnail
                  file={file}
                  agentId={agentId as string}
                  onOpen={openEntry}
                />
              ) : (
                <DeliverableChip file={file} onOpen={openEntry} />
              )}
            </DeliverableLinkCopy>
          ))}
        {docs.map((doc) => (
          <DeliverableLinkCopy key={doc.docId} text={doc.path}>
            <DeliverableDocCard doc={doc} onOpen={openDoc} />
          </DeliverableLinkCopy>
        ))}
      </div>
    </div>
  );
}

/**
 * The hover-revealed copy affordance shared by all three card kinds.
 *
 * What lands on the clipboard is the reference the platform already passes
 * around: a document's library path (the `ref` `doc_read` takes, and the value
 * `pm_submit_task`'s `doc_refs` accepts) or a workspace-relative file path.
 * That is what makes the copied text usable when pasted into another agent's
 * conversation — a clickable viewer link would only work inside this app.
 *
 * Hidden until the pointer arrives, so it never takes a click meant for the
 * card, and held open through the copy confirmation (`CopyButton`'s
 * `onCopiedChange`): a checkmark that fades on mouse-out reports nothing.
 *
 * The reveal is an **opaque overlay**, not a reserved gutter: hovering a card
 * washes it to `surface-hover`, and spending permanent padding on every chip
 * for a control that only exists while the pointer is there wasted width for
 * nothing. The slot therefore carries the opaque `bg-surface` itself — that is
 * load-bearing. The button's only background is its own hover tint
 * (`hover:bg-accent/10`, translucent, and a hover variant beats a
 * same-specificity `bg-surface` whatever the class order), so passing
 * `bg-surface` to `CopyButton` painted nothing and the chip's name and size
 * showed straight through the icon. What it covers while open is the trailing
 * size / version text, which is exactly what the tooltip repeats.
 */
function DeliverableLinkCopy({ text, children }: { text: string; children: React.ReactNode }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);
  return (
    <div className="group/link relative inline-flex">
      {children}
      <span
        className={cn(
          "absolute bottom-0.5 right-0.5 rounded bg-surface transition-opacity",
          copied
            ? "opacity-100"
            : "pointer-events-none opacity-0 group-hover/link:pointer-events-auto group-hover/link:opacity-100",
          "focus-within:pointer-events-auto focus-within:opacity-100",
        )}
      >
        <CopyButton
          text={text}
          iconOnly
          ariaLabel={t("deliverables.copyLink")}
          onCopiedChange={setCopied}
          testId="deliverable-copy-link"
        />
      </span>
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
  const name = deliverableFileName(file);
  // The editor preview's own hook, so the thumbnail and the full-size tab share
  // one LRU entry and one download.
  const { blobUrl, error } = useWorkspaceFileBlob(
    { agentId, workspaceId: file.workspaceId, relPath: file.relPath },
    { mode: "url" },
  );

  // A thumbnail we cannot load is still a file the user can open: fall back to
  // the chip rather than leaving a broken-image hole.
  if (error) {
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
      {blobUrl ? (
        <img src={blobUrl} alt={name} className="h-full w-full object-cover" />
      ) : (
        <span className="flex h-full w-full items-center justify-center">
          <Loader2 className="h-4 w-4 animate-spin text-text-tertiary" aria-hidden="true" />
        </span>
      )}
    </button>
  );
}

/** A capsule for a non-image workspace deliverable (a PDF, a report, a data
 *  file). Uses the same Seti file-type icon as the right-hand workspace tree,
 *  so a `.pdf`, a `.csv` and a `.md` are distinguishable at a glance. */
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
  const icon = getFileIcon(name);
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
      <SetiIcon name={icon.name} size={14} className="shrink-0" />
      <span className="truncate">{name}</span>
      <span className="shrink-0 text-text-tertiary">
        {formatDeliverableSize(file.bytes)}
      </span>
    </button>
  );
}

/** A card for an online doc-service document. Clicking opens a read-only
 *  Markdown preview tab; the markdown Seti icon matches the workspace tree. */
function DeliverableDocCard({
  doc,
  onOpen,
}: {
  doc: DeliverableDoc;
  onOpen: (doc: DeliverableDoc) => void;
}) {
  const name = deliverableDocFileName(doc);
  return (
    <button
      type="button"
      onClick={() => onOpen(doc)}
      title={doc.path}
      className={cn(
        "flex max-w-[16rem] items-center gap-1.5 rounded-md border border-border-outer",
        "bg-surface px-2 py-1.5 text-xs text-text-secondary",
        "hover:border-[var(--color-accent)]/40 hover:bg-surface-hover",
      )}
    >
      <SetiIcon name={getFileIcon(name).name} size={14} className="shrink-0" />
      <span className="truncate">{name}</span>
      {doc.version > 0 && (
        <span className="shrink-0 text-text-tertiary">v{doc.version}</span>
      )}
    </button>
  );
}