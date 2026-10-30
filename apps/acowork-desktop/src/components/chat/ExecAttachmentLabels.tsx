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
 * `AttachmentChipRow` draws. What differs is interactivity, and that is decided
 * by the data rather than by the surface — a label has no path to open, so it
 * renders the capsule's decorative form. The opposite of the downloadable
 * {@link ChatAttachmentChip}, which is the same capsule with a click.
 *
 * A turn sent with a file and no words paints these alone, above where the
 * bubble would be, so the head of the round is never a blank row.
 */
import type { ExecAttachmentLabel } from "../../stores/chatSessionStore";
import { cn } from "../../lib/utils";
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
}

export function ExecAttachmentLabels({ labels = NO_LABELS, align = "end" }: ExecAttachmentLabelsProps) {
  if (labels.length === 0) return null;
  return (
    <div
      className={cn(
        "mb-1 flex flex-wrap gap-1.5",
        align === "end" ? "justify-end self-end" : "justify-start",
      )}
    >
      {labels.map((label, i) => (
        <ExecAttachmentChip key={`${label.kind ?? ""}:${label.filename}:${i}`} label={label} />
      ))}
    </div>
  );
}

function ExecAttachmentChip({ label }: { label: ExecAttachmentLabel }) {
  const { Icon, className } = attachmentGlyph(label.kind);
  const range = selectionRange(label.startLine, label.endLine);
  const size = label.sizeBytes !== undefined ? formatAttachmentSize(label.sizeBytes) : undefined;
  return (
    <AttachmentCapsule title={label.filename}>
      <Icon className={cn("h-3.5 w-3.5 shrink-0", className)} />
      <span className="max-w-[12rem] truncate font-medium">{label.filename}</span>
      {range && <span className="text-text-tertiary">{range}</span>}
      {size && <span className="text-text-tertiary">{size}</span>}
    </AttachmentCapsule>
  );
}
