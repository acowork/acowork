/**
 * How the initiator-side bubble renders, given WHO the initiator is.
 *
 * The rule this module owns is the house grammar every chat surface already
 * states — "human input is literal text, markdown is agent speech" (see
 * `GroupChatPanel`'s own-row branch and its regression test) — but the
 * execution surface had been applying it by ROLE instead of by SPEAKER: an
 * execution session writes every inbound turn as a `user` conversation entry,
 * so when the initiator is an Agent (agent-to-agent intent work, ADR-076
 * §决策 4) its task brief — which is markdown, e.g. a PM dispatch body with
 * `##` headings and numbered steps — landed in the literal-text branch and
 * painted its own syntax as characters.
 *
 * Two facts travel together and MUST stay together, which is why they are one
 * module and not two ternaries:
 *
 *   1. **Renderer.** Agent speech goes through the markdown shell, human input
 *      stays literal. Rendering markdown inside the accent-filled human bubble
 *      is a known trap: the `prose` shell declares `color:
 *      var(--tw-prose-body)` (a page-level near-black) on its own wrapper,
 *      which beats the white `text-chat-user-text` the bubble inherits — the
 *      "user bubble text turned black" defect `GroupChatPanel.userLiteral`
 *      pins. So an agent's bubble takes the agent skin as well as the parser.
 *
 *   2. **Skin.** `bg-chat-user` + white text for a person, `bg-chat-bubble` +
 *      primary text for an Agent. Only the COLORS follow the speaker: the
 *      bubble stays right-aligned and keeps its `rounded-br-sm` tail, because
 *      the side encodes "this is the session's initiator", not "this is a
 *      human". Flipping the side would destroy the IM grammar of the surface.
 *
 * Like `bubbleMetaAnchor.ts` and `avatarAnchor.ts`, this is a pure function in
 * its own module so the render paths cannot drift apart again, and so the rule
 * is testable without mounting React (jsdom performs no layout and computes no
 * styles, which is why the rendered tests check DOM shape, not painted color).
 */

import type { InitiatorKind } from "../../lib/sessionInitiator";
import type { ChatMessage } from "../../lib/types";

/** Who is speaking on the initiator side of a session. `unknown` is the
 *  conservative value: the id resolved to neither the agent registry nor the
 *  account directory (an uninstalled initiator, or a registry still loading),
 *  so the surface has no fact to act on and keeps the literal rendering. It is
 *  the same vocabulary `sessionInitiator` resolves an id into — one fact, one
 *  name. */
export type SpeakerKind = InitiatorKind;

/** Background + text color of the initiator-side bubble. */
export function speakerBubbleClass(kind: SpeakerKind): string {
  return kind === "agent"
    ? "bg-chat-bubble text-text-primary"
    : "bg-chat-user text-chat-user-text";
}

/** Whether the initiator-side body is parsed as markdown. Only an Agent's
 *  speech is: a person's words are shown exactly as typed. */
export function speakerRendersMarkdown(kind: SpeakerKind): boolean {
  return kind === "agent";
}

/** The machine envelope `intent_turn::render_intent_message` writes as an
 *  agent-origin turn's row body: `[intent:from:action] …` for a request,
 *  `[intent_response:from:action] …` for the answer. The `from` is the only
 *  place the SENDER survives on the row — the display projection beside it
 *  (`metadata.intent`) carries the words, not who sent them. */
const INTENT_SENDER_PREFIX = /^\s*\[intent(?:_response)?:([^:\]\s]+):/;

/**
 * The agent that actually SPOKE this turn, when that is not the session's
 * initiator — else `null`.
 *
 * Every inbound turn of an execution session is written as a `user` row, and
 * the surface draws that row on the initiator's side with the initiator's name
 * — correct for the initiator's own words, wrong for the two peer turns an
 * agent-to-agent exchange puts there: the request (in the responder's session)
 * and the answer (in the initiator's session, routing itself back as
 * `kind=response`). In a human-run session that painted a peer agent's whole
 * answer as the human's own message.
 *
 * The rule needs two facts, both already on the row, and it must not guess:
 *
 *   1. a display projection exists — only an agent origin has one (design 27
 *      §5.4.5), so a row without it is the initiator speaking and stays put.
 *      It is also what gives the bubble something readable to paint; a legacy
 *      row that predates the projection keeps the old rendering.
 *   2. the envelope's `from` names somebody other than the initiator. When the
 *      initiator is a person the bubble renderer passes `null` and every agent
 *      origin is somebody else — which is the whole point.
 *
 * Pure, so the rule is checkable without React; `PeerAgentBubble` is the one
 * surface that consumes it (`MessageBubble`'s user branch).
 */
export function peerSpeakerId(
  message: Pick<ChatMessage, "content" | "display">,
  initiatorAgentId: string | null | undefined,
): string | null {
  if (!message.display) return null;
  const from = INTENT_SENDER_PREFIX.exec(message.content)?.[1];
  if (!from) return null;
  return from === initiatorAgentId ? null : from;
}
