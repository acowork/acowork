/**
 * RunAnswerBubble — the assistant half of an execution block's card.
 *
 * Why this exists: the run's latest answer is drawn in the CARD the block's
 * collapse header titles (the header is the card's title banner). It is the
 * Chat's own grammar — the agent's newest words sit in the transcript while
 * the rounds stay one click away — but one message must read as one object,
 * so the answer wears the card's body, not a second bubble floating under it.
 * That is why this component owns only the BODY (padding, the fold's skin)
 * and no shell of its own: the shell and the title banner belong to
 * `ChatExecutionBlock` (the recipe lives in `lib/ui-styles.ts`, shared with
 * `InteractionCardShell`).
 *
 * Why the fold: an agent's answer is usually LONG, and an unclamped body under
 * every block turns the transcript into a wall of text: paging through the
 * conversation becomes scrolling past a wall, and the thing the reader
 * actually came for — the shape of the conversation — is invisible. The fold
 * itself lives in `CollapsibleChatContent`; this component owns only the skin.
 */

import { memo } from "react";
import { ChatMarkdown } from "./ChatMarkdown";
import { CollapsibleChatContent } from "./CollapsibleChatContent";
import { chatCardBody } from "../../lib/ui-styles";

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
      // The card's body: the same neutral fill an interaction card's body
      // wears, so the answer sits INSIDE the block rather than beside it. The
      // block's 0.9× type step is the title banner's, not this one — the
      // answer keeps the Chat's own font scale, exactly like any other
      // assistant message ("a message type with two sizes depending on the
      // surface is how a Chat stops reading as one conversation").
      className={`${chatCardBody} min-w-0 max-w-full select-text break-words`}
    >
      <CollapsibleChatContent text={content}>
        <ChatMarkdown content={content} />
      </CollapsibleChatContent>
    </div>
  );
});