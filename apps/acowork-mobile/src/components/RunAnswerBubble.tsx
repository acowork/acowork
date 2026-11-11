/**
 * RunAnswerBubble — the assistant bubble under an execution block, folded to
 * eight lines.
 *
 * The mobile port of Desktop's `chat/RunAnswerBubble`. Why this exists: the
 * run's latest answer is drawn BELOW the execution block as an ordinary
 * assistant bubble (the Chat's own grammar — the agent's newest words sit in
 * the transcript while the rounds stay one tap away). But an agent's answer
 * is usually LONG, and an unclamped bubble under every block turns a 390pt
 * transcript into a wall of text: paging through the conversation becomes
 * scrolling past a wall, and the thing the reader actually came for — the
 * shape of the conversation — is invisible.
 *
 * The fold itself lives in `CollapsibleChatContent` (shared with any bubble
 * that can outgrow its measure); this component owns only the BUBBLE — the
 * transcript's own skin around the folded answer.
 */
import { memo } from 'react'
import { CollapsibleChatContent } from './CollapsibleChatContent'
import { renderMarkdown } from '../lib/markdown'

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
      // The transcript's own peer-bubble skin. It sits in the block column,
      // so it hugs the block's measure exactly as the card above it does —
      // and it inherits the Chat's font scale, because it IS the Chat's
      // bubble class. A message type with two sizes depending on the surface
      // is how a Chat stops reading as one conversation.
      className="bubble bubble-peer run-answer-bubble"
    >
      <CollapsibleChatContent text={content}>
        <div className="run-answer-content">{renderMarkdown(content)}</div>
      </CollapsibleChatContent>
    </div>
  );
});
