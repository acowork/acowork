/**
 * The two sides of a speaker disclosure, as names a person can read.
 *
 * `speakerProvenance.ts` decides WHETHER a turn is voiced as somebody else;
 * this module decides what the badge then SAYS. The raw record carries ids
 * (a human account uid, an agent instance id) because that is what the write
 * path verified — but "babea202-… 代 31839e13-… 发言" tells a reader
 * nothing, so the ids are resolved to names before they are painted.
 *
 * The resolution is NOT a second lookup chain: `resolveInitiatorIdentity`
 * (`lib/sessionInitiator.ts`) already owns "agent → person → short id", and
 * two chains that must agree drift apart. This module only decides WHICH id
 * is the "me" case — the one the resolver deliberately answers with `null` —
 * and names it from the profile store.
 *
 * Why the name comes from the profile store and not from
 * `authStore.account.display_name`: the account is written once, at login, so
 * its `display_name` goes stale the moment the user renames themselves
 * (`ProfileTab` writes only the profile store). Every other surface that
 * names "you" reads the profile store for the same reason. The account's
 * `user_id` is still the right source for the ID — it never changes.
 *
 * Why the agent registry is consulted before the account directory: an
 * agent instance id is not a user id, and the directory answers only for
 * people — so an initiator that is an Agent resolves in the registry, and
 * the directory request is not paid for at all on an agent-initiated run.
 */
import { useMemo } from "react";
import { speakerDisclosure, type TurnSpeaker } from "./speakerProvenance";
import {
  resolveInitiatorIdentity,
  useUserDirectory,
} from "../../lib/sessionInitiator";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useAuthStore } from "../../stores/authStore";
import { useAgentStore } from "../../stores/agentStore";
import type { DirectoryUser } from "../../lib/types";
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
  /** The viewer's display name — the profile store's, which a rename
   *  updates live. */
  myName?: string | null;
  /** The agent registry, keyed by INSTANCE id. */
  agents: Record<
    string,
    { meta?: { name?: string; display_name?: string; agent_id?: string } }
  >;
  /** The account directory. */
  directory: readonly DirectoryUser[];
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

  // ONE lookup, shared with the initiator resolution: an agent answers from
  // the registry, a person from the directory, and an id neither registry
  // knows degrades to a short id rather than to nothing.
  const nameOf = (id: string): string => {
    const identity = resolveInitiatorIdentity(
      id,
      null,
      src.directory,
      (aid) => {
        const meta = src.agents[aid]?.meta;
        return meta ? { name: meta.name, display_name: meta.display_name } : undefined;
      },
    );
    return identity?.displayName ?? id.slice(0, 8);
  };

  // The author is whichever kind of party the record says wrote the row: a
  // person (usually the signed-in account) or an agent instance.
  const author =
    (src.me && disclosure.author === src.me ? src.myName : undefined) ||
    nameOf(disclosure.author);

  // The target is the session's initiator — an agent instance or a person.
  const target = nameOf(disclosure.standsInFor);

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
  const myName = useUserProfileStore((s) => s.profile.displayName);
  const agents = useAgentStore((s) => s.agents);
  const directory = useUserDirectory();

  return useMemo(
    () => resolveSpeakerNames(speaker, { me, myName, agents, directory }),
    [speaker, me, myName, agents, directory],
  );
}

/**
 * The badge that says WHO spoke and WHOSE voice it is in.
 *
 * Its own component so the registries are only consulted when a disclosure
 * exists: an ordinary turn — the overwhelming majority of rows — mounts
 * nothing and pays nothing.
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
