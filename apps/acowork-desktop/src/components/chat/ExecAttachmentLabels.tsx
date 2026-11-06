/**
 * Execution-surface attachment labels on a Chat row (ADR-100 D12).
 *
 * When a composer sends a turn, the files it hands the task stay in that run's
 * own workspace — never in the chat session's `files/` dir (design 27 §4.2.1,
 * ADR-009 §5). The durable `user_message` head row therefore carries a label
 * only: what the item was called, which arm of the ADR-046 union it came from,
 * and the decoration a chip needs.
 *
 * This is the Chat surface's half of one grammar, not a second one: the capsule
 * frame comes from {@link AttachmentCapsule} and the glyph from
 * `attachmentGlyph`, the same two pieces the execution session's
 * `AttachmentChipRow` draws.
 *
 * Interactivity is decided by the data, not by the surface — and since
 * ADR-100 D12 Rev. 2026-11 the data can carry it: a workspace arm ships its
 * `absPath`, so the chip opens the file exactly the way the same chip does in
 * the execution session (`openAttachedRef` → the in-app fileTab). A banner that
 * looked like a button and did nothing was the complaint that produced the
 * revision; the rule is "openable or plainly decorative", never "looks openable".
 *
 * Not openable, on purpose:
 *   - the upload arms, whose bytes belong to the run that received them (this
 *     projection has no `documentId` either, and the chat's own uploads are a
 *     different chip — the downloadable {@link ChatAttachmentChip});
 *   - `attached_folder`, because fileTab has no folder view yet (same rule the
 *     composer tray follows: a click with no destination is worse than none);
 *   - a row written before the path existed, which is a label and nothing else.
 *
 * A turn sent with a file and no words paints these alone, above where the
 * bubble would be, so the head of the round is never a blank row.
 */
import type { ExecAttachmentLabel } from "../../stores/chatSessionStore";
import type { AttachedItem } from "../../lib/types";
import { cn } from "../../lib/utils";
import { openAttachedRef } from "../../lib/openWorkspaceRef";
import { AttachmentCapsule, formatAttachmentSize } from "./AttachmentCapsule";
import { attachmentGlyph, selectionRange } from "./attachmentGlyph";

/** Stable default for a row that carried nothing. Each call site writing its
 *  own `?? []` would hand every row a fresh array identity per render — the
 *  transcript's rows are memoized, so that is a re-render per row per tick. */
const NO_LABELS: ExecAttachmentLabel[] = [];

interface ExecAttachmentLabelsProps {
  /** Absent on every row that was not sent with items. */
  labels?: ExecAttachmentLabel[];
  /** Which side the speaker's chips hug: `end` on the human's own rows (the
   *  1:1 user row and a group's own row), `start` when another participant's
   *  pinned turn is replayed from the peer side. */
  align?: "end" | "start";
  /** The Agent whose workspaces a click resolves against. Absent (or a label
   *  with no path) leaves the chip decorative — the same degradation the
   *  composer tray chip has when no Agent is selected. */
  agentId?: string | null;
}

/**
 * The wire arm a label opens as, or `null` when there is nothing behind it.
 *
 * One table, so a new arm is a row here rather than a branch in the chip: the
 * three workspace arms map to the `AttachedItem` shape `openAttachedRef`
 * already accepts, and the upload arms (`file_upload` / `image_upload`) plus a
 * path-less row map to nothing.
 */
function openableItemOf(label: ExecAttachmentLabel): AttachedItem | null {
  if (!label.absPath) return null;
  switch (label.kind) {
    case "attached_file":
      return { type: "attached_file", absPath: label.absPath, name: label.filename };
    case "attached_selection":
      return {
        type: "attached_selection",
        absPath: label.absPath,
        name: label.filename,
        startLine: label.startLine ?? 1,
        endLine: label.endLine ?? 1,
      };
    default:
      // Uploads (bytes belong to the run) and folders (no fileTab view yet).
      return null;
  }
}

export function ExecAttachmentLabels({
  labels = NO_LABELS,
  align = "end",
  agentId,
}: ExecAttachmentLabelsProps) {
  if (labels.length === 0) return null;
  return (
    <div
      className={cn(
        "mb-1 flex flex-wrap gap-1.5",
        align === "end" ? "justify-end self-end" : "justify-start",
      )}
    >
      {labels.map((label, i) => (
        <ExecAttachmentChip
          key={`${label.kind ?? ""}:${label.filename}:${i}`}
          label={label}
          agentId={agentId ?? null}
        />
      ))}
    </div>
  );
}

function ExecAttachmentChip({
  label,
  agentId,
}: {
  label: ExecAttachmentLabel;
  agentId: string | null;
}) {
  const { Icon, className } = attachmentGlyph(label.kind);
  const range = selectionRange(label.startLine, label.endLine);
  const size = label.sizeBytes !== undefined ? formatAttachmentSize(label.sizeBytes) : undefined;
  const item = openableItemOf(label);
  // The chat surface has no execution session of its own, so there is no
  // priority workspace root: the resolver searches every workspace of the
  // Agent, which is what the composer tray chip does on this surface too.
  const onClick =
    item && agentId
      ? () => void openAttachedRef({ item, agentId, currentWorkspaceId: "" })
      : undefined;
  return (
    <AttachmentCapsule onClick={onClick} title={label.filename} ariaLabel={label.filename}>
      <Icon className={cn("h-3.5 w-3.5 shrink-0", className)} />
      <span className="max-w-[12rem] truncate font-medium">{label.filename}</span>
      {range && <span className="text-text-tertiary">{range}</span>}
      {size && <span className="text-text-tertiary">{size}</span>}
    </AttachmentCapsule>
  );
}
