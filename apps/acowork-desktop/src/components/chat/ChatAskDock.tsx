/**
 * `ChatAskDock` — the router's question, parked at the bottom of the surface.
 *
 * The question used to be drawn at the run's tail. The run is anchored where
 * its exchange STARTED (ADR-106 D3), so that tail can be hundreds of rows
 * above the composer, in a transcript that does not scroll on its own: the
 * human typed at the bottom and saw nothing happen. The dock floats the
 * question over the composer's edge instead — always on screen, always at the
 * bottom, with a gap to the input so nothing is covered.
 *
 * It is a POSITION, not a card: the question itself is still the shared
 * `ChatNoticeEventRow`, with the same answer path and the same wording, so a
 * docked question and a card drawn in the transcript cannot drift apart. The
 * run it asks about wears a badge (`ChatExecutionBlock`), which is what keeps
 * the question's subject attached to it now that the two are apart.
 *
 * The host owns the position, so the host owns this component's placement: it
 * renders inside the composer's `relative` box, and `bottom-full` is what
 * parks it above the input.
 */
import { memo } from "react";
import { ChatNoticeEventRow } from "./ChatNoticeEventRow";
import type { CardChoice } from "../../lib/chatCardAnswer";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

export interface ChatAskDockProps {
  /** The open question to dock, or none. */
  card: ChatMessageRow | null;
  /** Answer it — already gated by the host (`cardAnswerFor`), so absent means
   *  "this viewer may not answer it", not "the card cannot be drawn". */
  onChoose?: (choice: CardChoice) => void;
}

/** Memoized: the host re-renders on every composer keystroke, and the dock
 *  holds a live question — re-drawing it per keystroke would cost the card's
 *  own busy state nothing but time. */
export const ChatAskDock = memo(function ChatAskDock({
  card,
  onChoose,
}: ChatAskDockProps) {
  if (!card) return null;
  return (
    <div
      data-testid="chat-ask-dock"
      className="pointer-events-none absolute inset-x-0 bottom-full z-20 mb-3 flex justify-center px-4"
    >
      {/* One panel, the same chrome the address picker wears above the
          composer: the card's own hairline stays the card's, this is the
          elevation the whole thing floats on. */}
      <div className="pointer-events-auto rounded-lg bg-modal-surface p-2 shadow-xl">
        <ChatNoticeEventRow row={card} identity="floating" onChoose={onChoose} />
      </div>
    </div>
  );
});
