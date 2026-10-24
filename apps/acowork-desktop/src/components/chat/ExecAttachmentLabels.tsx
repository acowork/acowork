/**
 * Execution-surface attachment labels (ADR-100 D12).
 *
 * When an *execution* view's composer sends a turn, the files it hands the task
 * stay in that run's own workspace — never in the chat session's `files/` dir
 * (design 27 §4.2.1, ADR-009 §5). The durable `user_message` head row therefore
 * carries a label only: what the item was called on the sending surface, plus
 * which arm of the ADR-046 `AttachedItem` union it came from.
 *
 * There is nothing to open, so this is deliberately a muted, non-interactive
 * pill — the opposite of the downloadable {@link ChatAttachmentChip} — and it
 * follows that chip's grammar: a turn sent with a file and no words paints the
 * labels alone, above where the bubble would be, so the head of the round is
 * never a blank row.
 */
import type { ExecAttachmentLabel } from "../../stores/chatSessionStore";
import { cn } from "../../lib/utils";

interface ExecAttachmentLabelsProps {
  labels: ExecAttachmentLabel[];
  /** Which side the speaker's chips hug: `end` on the human's own rows (the
   *  1:1 user row and a group's own row), `start` when another participant's
   *  pinned turn is replayed from the peer side. */
  align?: "end" | "start";
}

export function ExecAttachmentLabels({ labels, align = "end" }: ExecAttachmentLabelsProps) {
  if (labels.length === 0) return null;
  return (
    <div
      className={cn(
        "mb-1 flex flex-wrap gap-1.5",
        align === "end" ? "justify-end self-end" : "justify-start",
      )}
    >
      {labels.map((label, i) => (
        <span
          key={`${label.kind ?? ""}:${label.filename}:${i}`}
          title={label.filename}
          className="inline-flex items-center gap-1 rounded-md border border-dashed border-border-outer bg-zinc-50 px-2 py-0.5 text-xs text-text-tertiary select-text dark:bg-zinc-800"
        >
          {label.kind && <span className="text-10 uppercase leading-none">{label.kind}</span>}
          <span className="max-w-[12rem] truncate">{label.filename}</span>
        </span>
      ))}
    </div>
  );
}
