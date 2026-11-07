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
