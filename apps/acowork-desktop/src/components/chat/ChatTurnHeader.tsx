/**
 * ChatTurnHeader — the turn header of a chat transcript (ADR-106 revision
 * 2026-11-08): the identity line a surface shows while its OWN send is
 * un-answered, in place of the old centered wait line.
 *
 * The execution surface answers "I sent something, it is being worked on"
 * with the assistant's avatar + name + a live phase word, immediately; the
 * chat surfaces could only offer a centered generic line until the round's
 * reference row landed, because naming a responder used to be a guess read
 * off the transcript. The revision narrows that ban to what it was always
 * about — inferring the responder from transcript SHAPE — and allows the
 * delivery decision's own inputs: the 1:1 peer, the `@`-mention list, the
 * round a reply addresses, the group's default responder. A re-route or a
 * failed delivery is stated by a server-side closing row, and a closing row
 * ends the claim this header rides, so a wrong name cannot outlive the wait
 * line it replaced.
 *
 * Shape follows the block header it hands over to (`ChatExecutionBlock`):
 * same gutter, same 40px avatar, same name tier — the takeover happens at
 * the same anchor, so "content overwrites the state" reads as one column
 * that stops shimmering, not as a jump. Multiple responders (a fan-out)
 * wear ONE stacked header: one send is one fact.
 *
 * What this component does NOT do: it never renders a nameless avatar (the
 * caller degrades to `ChatTurnStatusLine` when identities do not resolve),
 * and it never asserts a phase it was not given — the status word is the
 * caller's localized text, generic "working" until a live phase is
 * available (D9: the claim is a fact about the window, not about a phase).
 */
import { cn } from "../../lib/utils";
import { AgentAvatar } from "../common/AgentAvatar";
import { useTranslation } from "../../i18n/useTranslation";

/** One expected responder, resolved by the surface through the same lookups
 *  its bubbles use (agent registry / account directory). */
export interface TurnIdentity {
  /** Instance id — the avatar seed. */
  id: string;
  displayName: string;
  avatarUrl?: string | null;
  builtinAvatarId?: string | null;
  version?: string | null;
}

interface ChatTurnHeaderProps {
  /** Non-empty; an empty list renders nothing (the caller falls back to the
   *  generic centered line — a header without a face is that line's job). */
  identities: readonly TurnIdentity[];
  /** Already-localized status text (generic "working…" while the claim
   *  carries no live phase). */
  text: string;
  /** Surface column gutter so the header lines up with the stream. */
  className?: string;
}

export function ChatTurnHeader({ identities, text, className }: ChatTurnHeaderProps) {
  const { i18n } = useTranslation();
  if (identities.length === 0) return null;
  const single = identities.length === 1;
  // The names are a list, and a list has a locale: "A、B" in zh and ja,
  // "A, B" in en, "A 및 B" in ko. `Intl.ListFormat` is the platform's own
  // rule — a hand-picked separator would be one more thing to drift.
  const names = new Intl.ListFormat(i18n.language, {
    type: "conjunction",
    style: "narrow",
  }).format(identities.map((p) => p.displayName));
  return (
    <div
      // The handover hook: tests and the takeover rule key on this, the same
      // way `[data-event-text]` pins the axis of the centered lines.
      data-turn-header=""
      role="status"
      aria-live="polite"
      className={cn("flex items-center gap-2 px-4 py-1", className)}
    >
      {single ? (
        <AgentAvatar
          agentId={identities[0].id}
          displayName={identities[0].displayName}
          avatarUrl={identities[0].avatarUrl ?? null}
          version={identities[0].version ?? null}
          builtinAvatarId={identities[0].builtinAvatarId ?? null}
          size={40}
          className="shrink-0"
        />
      ) : (
        // Fan-out: each face after the first slides under the one before it,
        // and the earlier face paints on top. The footprint is 40px + 16px per
        // extra responder — it grows, but the stack's LEFT edge stays where
        // the block header takes over, which is the anchor the handover rule
        // is about. The layer and the offset are a function of the position,
        // so they are inline styles rather than a class table that would stop
        // working at the fourth mention. The rounding and the hairline ring
        // come from `AgentAvatar` itself: this is the same face the transcript
        // already wears, not a second avatar style.
        <span className="flex shrink-0">
          {identities.map((p, i) => (
            <span
              key={p.id}
              className="relative"
              style={{ zIndex: identities.length - i, marginLeft: i === 0 ? 0 : -24 }}
            >
              <AgentAvatar
                agentId={p.id}
                displayName={p.displayName}
                avatarUrl={p.avatarUrl ?? null}
                version={p.version ?? null}
                builtinAvatarId={p.builtinAvatarId ?? null}
                size={40}
              />
            </span>
          ))}
        </span>
      )}
      <div className="flex min-w-0 flex-col">
        <span className="truncate text-xs font-medium text-text-tertiary">{names}</span>
        {/* The claim's word, on the same tier as the centered lines' — the
            `thinking-shimmer` is what "the platform is still waiting" reads
            like everywhere else in the transcript. */}
        <span className="text-10 leading-tight text-text-tertiary thinking-shimmer">
          {text}
        </span>
      </div>
    </div>
  );
}
