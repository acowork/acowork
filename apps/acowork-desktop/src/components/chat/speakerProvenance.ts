/**
 * How a turn discloses that somebody spoke in someone else's voice.
 *
 * An execution session belongs to its **initiator**. Since the intervention
 * tier exists (plan D1/D2), a human may type into a session they did not start —
 * most importantly into an agent-to-agent run whose only other writer is an
 * agent that has already been told the work failed. The Runtime records who
 * actually spoke, on the row itself (`metadata.speaker` → `ConversationEntry.
 * speaker` / `RoundProjection.user.speaker`), and this module is the one place
 * that turns that fact into what a person reads.
 *
 * Two rules, both learned from the surfaces around it:
 *
 *   1. **Absence is not "unknown".** A turn with no `speaker` is the ordinary
 *      case (the initiator typing into their own session), and it must render
 *      exactly as it did before this existed — no badge, no placeholder. The
 *      Runtime already folds "no key" / "null" / "unusable" into `undefined`, so
 *      this module never has to guess.
 *
 *   2. **Only a substitution is disclosed.** A human who joined the session in
 *      their own right (mode 3) speaks as themselves: `author` is set and
 *      `on_behalf_of` is not, and a badge would be noise on every line they ever
 *      write. `on_behalf_of` is the one field that means "these words are being
 *      voiced as somebody else", so it is the only trigger.
 *
 * Pure and testable without mounting React, like `speakerBubble.ts` and
 * `bubbleMetaAnchor.ts`, so the three surfaces that paint a user row (execution
 * bubble, attachment bubble, Chat execution block) cannot drift apart.
 */

/** The provenance record the Runtime writes on a `user` row (plan D3).
 *
 *  Mirrors `acowork_core::turn_speaker::TurnSpeaker`. `on_behalf_of` is absent
 *  rather than `null` when nobody stood in for the author — the serializer omits
 *  it, and a reader must treat absent and `null` alike. */
export interface TurnSpeaker {
  origin: "human" | "agent";
  /** The account (or originating agent instance) that wrote the row. */
  author?: string;
  /** The initiator these words are voiced as, when that is not the author. */
  on_behalf_of?: string;
}

/** What a surface should say about a speaker, or `null` for "say nothing". */
export interface SpeakerDisclosure {
  /** The person who typed the line. */
  author: string;
  /** The party whose voice it uses. */
  standsInFor: string;
}

/**
 * The disclosure for one turn, or `null` when there is nothing to disclose.
 *
 * Returns `null` for: no speaker (ordinary turn), a speaker with no
 * substitution (a participant in their own voice), and a substitution whose
 * author is empty (a partial row — an unnamed voice is not a disclosure a person
 * can act on). Both sides are required for a badge to mean anything.
 */
export function speakerDisclosure(
  speaker?: TurnSpeaker | null,
): SpeakerDisclosure | null {
  if (!speaker) return null;
  const standsInFor = (speaker.on_behalf_of ?? "").trim();
  if (!standsInFor) return null;
  const author = (speaker.author ?? "").trim();
  if (!author) return null;
  // Author and target being the same string is not a substitution, whatever the
  // producer said: the row would otherwise claim a person stood in for
  // themselves.
  if (author === standsInFor) return null;
  return { author, standsInFor };
}

/**
 * Is this turn spoken in someone else's voice?
 *
 * The same predicate the badge uses, exposed for surfaces that change more than
 * the badge (the Chat block swaps the row's attribution line, for instance).
 */
export function isVoicedForAnother(
  speaker?: TurnSpeaker | null,
): boolean {
  return speakerDisclosure(speaker) !== null;
}
