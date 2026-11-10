/**
 * The two sides of a speaker disclosure, as names a person can read.
 *
 * `speakerProvenance.ts` decides WHETHER a turn is voiced as somebody else;
 * this module decides what the badge then SAYS. The raw record carries ids
 * (a human account uid, an agent instance id) because that is what the write
 * path verified — but "babea202-… 代 31839e13-… 发言" tells a reader
 * nothing, so the ids are resolved to names before they are painted.
 *
 * The lookups are the ones the app already owns:
 *   - the agent registry (`agentStore.agents`, keyed by instance id) for an
 *     agent — the same registry the initiator resolution reads
 *     (`lib/sessionInitiator.ts`);
 *   - the account directory (`/api/users/directory`) for a person;
 *   - the signed-in account for the author of a turn the viewer wrote.
 *
 * A name that cannot be resolved falls back to a SHORT id (`slice(0, 8)`) —
 * the same degradation `resolveInitiatorIdentity` uses, so an unknown party
 * is still identifiable without being unreadable.
 */
import { useMemo } from "react";
import { speakerDisclosure, type TurnSpeaker } from "./speakerProvenance";
import { useUserDirectory } from "../../lib/sessionInitiator";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import { useTranslation } from "../../i18n/useTranslation";

/** The names of a disclosure's author and of the party it speaks for. */
export interface SpeakerNames {
  /** Who typed the line. */
  author: string;
  /** Whose voice the line is in. */
  target: string;
}

/** The registries the lookup reads. */
export interface SpeakerNameSources {
  /** The signed-in account id (the viewer's own id), if any. */
  me?: string | null;
  /** The signed-in account's display name. */
  myName?: string | null;
  /** The agent registry, keyed by INSTANCE id. */
  agents: Record<
    string,
    { meta?: { name?: string; display_name?: string; agent_id?: string } }
  >;
  /** The account directory. */
  directory: readonly {
    user_id: string;
    username?: string;
    display_name?: string;
  }[];
}

/** Shorten an id that no registry answered for. */
function shortId(id: string): string {
  return id.slice(0, 8);
}

/**
 * The display names for a disclosure's two sides, or `null` when there is no
 * disclosure (nothing to name).
 *
 * Pure, so the resolution rules are checkable without React.
 */
export function resolveSpeakerNames(
  speaker: TurnSpeaker | null | undefined,
  src: SpeakerNameSources,
): SpeakerNames | null {
  const disclosure = speakerDisclosure(speaker);
  if (!disclosure) return null;

  const agentName = (id: string): string | undefined => {
    const meta = src.agents[id]?.meta;
    return meta?.display_name || meta?.name || meta?.agent_id || undefined;
  };
  const personName = (id: string): string | undefined => {
    const user = src.directory.find((u) => u.user_id === id);
    return user?.display_name || user?.username || undefined;
  };

  // The author is whichever kind of party the record says wrote the row: a
  // person (usually the signed-in account) or an agent instance.
  const author =
    (src.me && disclosure.author === src.me ? src.myName : undefined) ||
    agentName(disclosure.author) ||
    personName(disclosure.author) ||
    shortId(disclosure.author);

  // The target is the session's initiator — an agent instance or a person.
  const target =
    agentName(disclosure.standsInFor) ||
    personName(disclosure.standsInFor) ||
    shortId(disclosure.standsInFor);

  return { author, target };
}

/**
 * The names for a disclosure, resolved against the live registries.
 *
 * A hook rather than a lookup at paint time: the registries arrive
 * asynchronously (the directory is fetched, the agent list is polled), so the
 * name lands on a later render rather than never.
 */
export function useSpeakerNames(
  speaker?: TurnSpeaker | null,
): SpeakerNames | null {
  const me = useAuthStore((s) => s.account?.user_id ?? null);
  const myName = useAuthStore((s) => s.account?.display_name ?? null);
  const agents = useAgentStore((s) => s.agents);
  // The directory is the LAST resort (the agent registry and the signed-in
  // account answer first), so it is only FETCHED when a lookup actually needs
  // it — an ordinary turn, or a disclosure whose both sides the registry
  // already names, never pays for the request.
  const needDirectory = needsDirectory(speaker, { me, myName, agents });
  const directory = useUserDirectory(needDirectory);

  return useMemo(
    () => resolveSpeakerNames(speaker, { me, myName, agents, directory }),
    [speaker, me, myName, agents, directory],
  );
}

/**
 * Would a lookup need the account directory to answer?
 *
 * The agent registry and the signed-in account are synchronous; the directory
 * is a network round-trip. This asks whether either side is still unnamed
 * after those two, which is exactly when the directory is worth fetching.
 */
function needsDirectory(
  speaker: TurnSpeaker | null | undefined,
  src: Omit<SpeakerNameSources, "directory">,
): boolean {
  const disclosure = speakerDisclosure(speaker);
  if (!disclosure) return false;
  const agentName = (id: string): string | undefined => {
    const meta = src.agents[id]?.meta;
    return meta?.display_name || meta?.name || meta?.agent_id || undefined;
  };
  const authorNamed =
    (src.me && disclosure.author === src.me ? src.myName : undefined) ||
    agentName(disclosure.author);
  const targetNamed = agentName(disclosure.standsInFor);
  return !authorNamed || !targetNamed;
}

/**
 * The badge that says WHO spoke and WHOSE voice it is in.
 *
 * Its own component so the registries are only consulted when a disclosure
 * exists: an ordinary turn — the overwhelming majority of rows — mounts
 * nothing, and the directory fetch is not paid for by every bubble in the
 * transcript.
 */
export function SpeakerBadge({
  speaker,
  className,
}: {
  speaker?: TurnSpeaker | null;
  className?: string;
}) {
  const { t } = useTranslation();
  const names = useSpeakerNames(speaker);
  if (!names) return null;
  return (
    <span className={className}>
      {t("chatPanel.speakerOnBehalfOf", {
        author: names.author,
        target: names.target,
      })}
    </span>
  );
}
