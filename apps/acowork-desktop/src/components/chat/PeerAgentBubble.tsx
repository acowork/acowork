/**
 * A PEER agent's turn inside somebody else's execution session.
 *
 * An agent-to-agent exchange reaches the initiator's session as an ordinary
 * `user` row (`[intent_response:from:action] …`), so the transcript drew it on
 * the initiator's side under the initiator's name — in a human-run session, the
 * human's own name over another agent's answer. `speakerBubble.peerSpeakerId`
 * decides WHEN a row is such a turn; this component draws it, and the one thing
 * it must get right is attribution: the sender's face and name, on the LEFT, the
 * side this surface gives every agent.
 *
 * The registries are consulted HERE rather than in `MessageBubble` on purpose —
 * the same reason `SpeakerBadge` exists: an ordinary row (the overwhelming
 * majority) mounts nothing and pays nothing, and only a peer row subscribes to
 * the agent registry / account directory. Identity resolution is
 * `lib/sessionInitiator`'s: a peer's sender id is just another id in the same two
 * registries, and an id neither knows degrades to a short id rather than to the
 * wrong person.
 */
import { memo } from "react";
import type { ChatMessage } from "../../lib/types";
import { useInitiatorIdentity } from "../../lib/sessionInitiator";
import { AgentAvatar } from "../common/AgentAvatar";
import { IntentDisplayBody } from "./IntentDisplayBody";
import { formatBubbleTime } from "../../lib/formatTime";

export const PeerAgentBubble = memo(function PeerAgentBubble({
  message,
  agentId,
}: {
  /** The inbound turn. Its `display` is the turn written for a person; the
   *  envelope in `content` stays one click away (it carries the coordinates). */
  message: ChatMessage;
  /** The sending agent's instance id, from `peerSpeakerId`. */
  agentId: string;
}) {
  const identity = useInitiatorIdentity(agentId);
  const display = message.display;
  if (!display) return null;
  const name = identity?.displayName ?? agentId.slice(0, 8);
  const fontSizeStyle = { fontSize: "var(--ui-font-size, 0.875rem)" };
  return (
    <div className="min-w-0 flex flex-col">
      {/* Who is speaking: the peer, not the session's initiator. Same 40px
          gutter and header grammar the agent's own rows use here. */}
      <div className="flex items-center gap-2 mb-1 mt-1">
        <AgentAvatar
          agentId={agentId}
          displayName={name}
          avatarUrl={identity?.avatarUrl ?? null}
          version={identity?.version ?? null}
          builtinAvatarId={identity?.builtinAvatarId ?? null}
          size={40}
          className="shrink-0"
        />
        <span className="text-xs font-medium text-text-tertiary">{name}</span>
      </div>
      <div className="group ml-12 flex flex-col items-start">
        <div
          className="max-w-[var(--content-max-width)] rounded-md rounded-bl-sm bg-chat-bubble px-4 py-2.5 select-text break-words"
          style={fontSizeStyle}
        >
          <IntentDisplayBody
            display={display}
            envelope={message.content}
            fontSize={fontSizeStyle}
          />
        </div>
        {message.timestamp ? (
          <span className="mt-1 text-10 text-text-tertiary opacity-0 transition-opacity group-hover:opacity-100">
            {formatBubbleTime(message.timestamp)}
          </span>
        ) : null}
      </div>
    </div>
  );
});