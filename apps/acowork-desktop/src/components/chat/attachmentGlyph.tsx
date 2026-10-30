/**
 * The one vocabulary table for "what an attachment looks like".
 *
 * Five components render attachments across the two chat-like surfaces: the
 * execution session's `AttachmentChipRow` and `DocumentChip`, the Chat
 * surface's `ChatAttachmentChip` and its execution-label chips, and the two
 * composers' pending trays. They agreed on nothing until now — the same
 * `attached_folder` was an amber `Folder` icon in one surface and a dashed
 * text pill in the other, and the Chat surface printed the wire's discriminant
 * (`ATTACHED_FILE`) as if it were a human label.
 *
 * The table is keyed by the ADR-046 tag because that tag is what every layer
 * already carries: the wire union, the Chat row's `AttachmentLabel` projection
 * (`acowork_core::protocol::AttachmentLabel`) and the router's prompt line all
 * quote it. One key, one glyph, one colour — a new attachment arm is a new row
 * here, not a new component.
 */
import { FileText, Folder, Hash, Image as ImageIcon, Upload, type LucideIcon } from "lucide-react";

export interface AttachmentGlyph {
  Icon: LucideIcon;
  /** Icon tint. Sizes differ because the chips do: refs sit in a compact row,
   *  uploads in a roomier capsule. */
  className: string;
}

/** ADR-046 `AttachedItem` tags, as they appear on the wire and in a Chat row. */
export const ATTACHMENT_KIND = {
  fileUpload: "file_upload",
  imageUpload: "image_upload",
  attachedFile: "attached_file",
  attachedSelection: "attached_selection",
  attachedFolder: "attached_folder",
} as const;

const TABLE: Record<string, AttachmentGlyph> = {
  [ATTACHMENT_KIND.attachedFile]: { Icon: FileText, className: "text-text-tertiary" },
  [ATTACHMENT_KIND.attachedSelection]: { Icon: Hash, className: "text-[var(--color-accent)]" },
  [ATTACHMENT_KIND.attachedFolder]: { Icon: Folder, className: "text-amber-500" },
  [ATTACHMENT_KIND.fileUpload]: { Icon: Upload, className: "text-text-tertiary" },
  [ATTACHMENT_KIND.imageUpload]: { Icon: ImageIcon, className: "text-text-tertiary" },
};

/** A row written before the projection carried a `kind`, or by a sender that
 *  did not say which arm it was: a plain file glyph, never a missing icon. */
const FALLBACK: AttachmentGlyph = { Icon: FileText, className: "text-text-tertiary" };

export function attachmentGlyph(kind?: string): AttachmentGlyph {
  if (!kind) return FALLBACK;
  return TABLE[kind] ?? FALLBACK;
}

/**
 * `L12-L25` / `L12` — the line range of a selection reference.
 *
 * The one spelling across every surface and the router prompt: the Chat row's
 * chip, the execution session's chip, the composer tray and the
 * `[attached refs]` line (`chat_router::context::refs_line`) all render
 * `L{start}-L{end}`, so one selection never reads two ways.
 */
export function selectionRange(startLine?: number, endLine?: number): string | undefined {
  if (startLine === undefined) return undefined;
  if (endLine === undefined || endLine === startLine) return `L${startLine}`;
  return `L${startLine}-L${endLine}`;
}
