/**
 * RunAnswerBubble — the run's latest answer, folded to eight lines, drawn as
 * the execution card's BODY.
 *
 * The mobile port of Desktop's `chat/RunAnswerBubble`. Why this exists: an
 * agent's answer is usually LONG, and an unclamped body under every block
 * turns a 390pt transcript into a wall of text: paging through the
 * conversation becomes scrolling past a wall, and the thing the reader
 * actually came for — the shape of the conversation — is invisible.
 *
 * It is the CARD's body, not a bubble floating under the block: the collapse
 * header above is the card's title banner, so the run and the agent's newest
 * words read as one object rather than two.
 *
 * The fold itself lives in `CollapsibleChatContent` (shared with any bubble
 * that can outgrow its measure); this component owns only the body — the
 * card's padding around the folded answer.
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
      // The card's BODY: the card's own padding and fill, so the answer sits
      // INSIDE the block rather than beside it. The block's 0.9× type step is
      // the title banner's, not this one — the answer keeps the Chat's own
      // font scale, exactly like any other assistant message ("a message type
      // with two sizes depending on the surface is how a Chat stops reading as
      // one conversation").
      className="run-answer-bubble"
    >
      <CollapsibleChatContent text={content}>
        <div className="run-answer-content">{renderMarkdown(content)}</div>
      </CollapsibleChatContent>
    </div>
  );
});
