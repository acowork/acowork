/**
 * RunAnswerBubble — the assistant bubble under an execution block, folded to
 * ten lines.
 *
 * Why this exists: the run's latest answer is drawn BELOW the execution block
 * as an ordinary assistant bubble (the Chat's own grammar — the agent's newest
 * words sit in the transcript while the rounds stay one click away). But an
 * agent's answer is usually LONG, and an unclamped bubble under every block
 * turns the transcript into a wall of text: paging through the conversation
 * becomes scrolling past a wall, and the thing the reader actually came for —
 * the shape of the conversation — is invisible.
 *
 * The fold itself lives in `CollapsibleChatContent` (shared with the group
 * chat's agent bubbles); this component owns only the BUBBLE — the
 * transcript's own skin around the folded answer.
 */

import { memo } from "react";
import { ChatMarkdown } from "./ChatMarkdown";
import { CollapsibleChatContent } from "./CollapsibleChatContent";

interface RunAnswerBubbleProps {
  /** The run's latest answer — the same projected `answer` the newest
   *  answered round carries. Nothing here can say anything the block would
   *  not. */
  content: string;
}

export const RunAnswerBubble = memo(function RunAnswerBubble({
  content,
}: RunAnswerBubbleProps) {
  return (
    <div
      data-run-answer=""
      // The transcript's own bubble skin, and its own measure: this sits in
      // the block column (`items-start`), so it hugs its text up to the
      // column's `--content-max-width` exactly as an agent row does, and it
      // inherits the Chat's font scale — the block's 0.9× step is the sibling
      // div above, not this one. A message type with two sizes depending on
      // the surface is how a Chat stops reading as one conversation.
      className="mt-2 min-w-0 max-w-full rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 select-text break-words"
    >
      <CollapsibleChatContent text={content}>
        <ChatMarkdown content={content} />
      </CollapsibleChatContent>
    </div>
  );
});
