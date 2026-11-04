/**
 * ADR-107: the reference block of a user turn.
 *
 * A reference is context the Runtime supplied that the human did **not** type —
 * today the chat background a Chat-Router-dispatched turn came from. It is a
 * message *part* (parallel to an attachment, ADR-046), so it renders inside the
 * user block, never as a message of its own, and never inside the human's
 * bubble: the bubble stays what the person typed, verbatim and un-rendered.
 *
 * Two rendering decisions live here, both deliberate:
 *
 * 1. **The body is markdown; the human's words are not.** Runtime-authored
 *    context is prose and goes through `ChatMarkdown` (tables, lists, code
 *    fences); a human's own message stays literal — the speaker-based split
 *    every surface already applies (`GroupChatPanel.userLiteral.test.tsx`
 *    records the defect that came from breaking it). The card is a neutral
 *    surface, so the `prose` shell's body colour is correct here, which is
 *    exactly why the accent-filled user bubble cannot use it.
 * 2. **Collapsed by default.** The point of the reference is that the human's
 *    message leads; a long background expanded above the agent's answer would
 *    push the answer off screen. One click opens it.
 *
 * Chrome follows `CompactionCard`: same pill header, same left-accent expanded
 * body with a capped height, so the two folded blocks read as one family.
 */
import { memo, useState } from "react";
import { ChevronDown, ChevronRight, Quote } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { ChatMarkdown } from "./ChatMarkdown";

/** Matches `CompactionCard`'s card/detail sizes so folded blocks carry equal
 *  visual weight. */
const CARD_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";

/** Label key per reference kind. An unknown kind (a newer producer) falls back
 *  to the generic label rather than rendering a raw identifier at the human. */
function labelKey(kind: string | undefined): string {
  switch (kind) {
    case "chat_background":
      return "messageRef.chatBackground";
    default:
      return "messageRef.generic";
  }
}

export interface MessageRefBlockProps {
  /** Reference kind — `metadata.kind` of the `context_ref` row. */
  kind?: string;
  /** Markdown body (the row's `content`). */
  content: string;
  /** Optional timestamp shown next to the label, same recipe as the summary
   *  card's detail row. Omit when the caller has none. */
}

export const MessageRefBlock = memo(function MessageRefBlock({
  kind,
  content,
}: MessageRefBlockProps) {
  const { t } = useTranslation();
  const [expanded, setExpanded] = useState(false);
  if (!content.trim()) return null;

  return (
    <div className="mt-1 flex w-full flex-col items-end">
      <button
        type="button"
        onClick={() => setExpanded((v) => !v)}
        aria-expanded={expanded}
        className="flex w-fit max-w-full items-center gap-2 rounded-md bg-zinc-50 px-2.5 py-1.5 text-text-tertiary transition-colors hover:bg-zinc-100 dark:bg-zinc-800/30 dark:hover:bg-zinc-800/50"
        style={{ fontSize: CARD_FONT_SIZE }}
      >
        <Quote className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        <span className="min-w-0 truncate font-medium">{t(labelKey(kind))}</span>
        {expanded ? (
          <ChevronDown className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        ) : (
          <ChevronRight className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        )}
      </button>

      {expanded && (
        <div
          className="mr-2 mt-1 w-full overflow-y-auto rounded-md border-r-2 border-zinc-300 bg-zinc-50 py-2 pl-2 pr-3 text-right dark:border-zinc-600 dark:bg-zinc-800/30"
          style={{ maxHeight: "240px" }}
        >
          <div className="text-left">
            <ChatMarkdown content={content} style={{ fontSize: CARD_FONT_SIZE }} />
          </div>
        </div>
      )}
    </div>
  );
});
