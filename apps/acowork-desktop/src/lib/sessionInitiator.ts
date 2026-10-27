//! ADR-076 §决策 4 — whose execution session is this?
//!
//! An execution session has exactly one initiator, and it is the only identity
//! the surface may show for its human side. `userProfileStore` holds whoever is
//! logged in *now*; reading the human identity from it (as the execution view
//! did) paints somebody else's session with the current account's name and
//! avatar the moment you switch accounts (t-9764c5fa).
//!
//! The initiator is a user id or an agent instance id, so the lookup has to try
//! both registries: the account directory for people, the agent registry for
//! agent-initiated work.

import { useEffect, useMemo, useState } from "react";
import { fetchDirectory } from "./auth-api";
import { getGatewayUrl } from "./config";
import type { DirectoryUser } from "./types";
import { useAuthStore } from "../stores/authStore";
import { useAgentStore } from "../stores/agentStore";

/** What the surface needs to render the initiator. */
export interface InitiatorIdentity {
  displayName: string;
  avatarUrl: string | null;
  builtinAvatarId: string | null;
}

/** The agent-registry fields the lookup uses (mirrors `AgentMeta`). */
export interface InitiatorAgent {
  name?: string;
  display_name?: string;
  avatar?: string | null;
  builtin_avatar?: string | null;
}

/**
 * Resolve an initiator id to a renderable identity, or `null` when the answer
 * is "that's me / nothing to say" and the caller should fall back to the
 * logged-in profile.
 *
 * Pure, so the three rules are testable without React:
 * 1. no initiator → `null` (a session from before the field existed);
 * 2. initiator is the logged-in account → `null` (the profile IS them);
 * 3. otherwise the directory (person) / registry (agent) decides, and an
 *    unknown id degrades to a short id rather than to the viewer.
 */
export function resolveInitiatorIdentity(
  initiator: string | null | undefined,
  me: string | null | undefined,
  directory: readonly DirectoryUser[],
  agentOf: (id: string) => InitiatorAgent | undefined,
): InitiatorIdentity | null {
  if (!initiator) return null;
  if (me && initiator === me) return null;

  const agent = agentOf(initiator);
  if (agent) {
    return {
      displayName: agent.display_name ?? agent.name ?? initiator.slice(0, 8),
      avatarUrl: agent.avatar ?? null,
      builtinAvatarId: agent.builtin_avatar ?? null,
    };
  }
  const user = directory.find((u) => u.user_id === initiator);
  if (user) {
    return {
      displayName: user.display_name || user.username,
      avatarUrl: user.avatar ?? null,
      builtinAvatarId: user.builtin_avatar ?? null,
    };
  }
  return { displayName: initiator.slice(0, 8), avatarUrl: null, builtinAvatarId: null };
}

// One directory fetch shared by every surface that needs to name a person.
// ponytail: process-lifetime cache, no invalidation — a rename shows up on the
// next app start. Upgrade path if that shows up: refetch on the same doorbell
// that already refreshes the Chat surfaces.
let directoryOnce: Promise<DirectoryUser[]> | null = null;

/** Exported only so the cache rules below can be checked without React. */
export function loadDirectory(): Promise<DirectoryUser[]> {
  if (!directoryOnce) {
    const token = useAuthStore.getState().accessToken;
    // No token = not logged in yet. Returning an empty list WITHOUT caching it:
    // caching here would pin every other participant to a short id for the rest
    // of the process, because the next call would find the promise already
    // resolved. Local mode has no directory to fetch, so the miss costs one
    // token read.
    if (!token) return Promise.resolve([]);
    directoryOnce = fetchDirectory(getGatewayUrl(), token).then(
      (d) => d,
      () => {
        // A failure is not an answer: clear the cache so the next mount
        // retries. Caching `[]` would render every other participant as a
        // short id for the rest of the process after one network blip.
        directoryOnce = null;
        return [];
      },
    );
  }
  return directoryOnce;
}

/** The account directory, fetched once per process. Empty while loading. */
export function useUserDirectory(): DirectoryUser[] {
  const [directory, setDirectory] = useState<DirectoryUser[]>([]);
  useEffect(() => {
    let alive = true;
    void loadDirectory().then((d) => {
      if (alive) setDirectory(d);
    });
    return () => {
      alive = false;
    };
  }, []);
  return directory;
}

/**
 * The identity to render for a session's human side, given its `initiator`.
 * `null` = "use the logged-in profile" (own session, or no initiator known).
 */
export function useInitiatorIdentity(
  initiator: string | null | undefined,
): InitiatorIdentity | null {
  const me = useAuthStore((s) => s.account?.user_id ?? null);
  const agents = useAgentStore((s) => s.agents);
  const directory = useUserDirectory();
  return useMemo(
    () =>
      resolveInitiatorIdentity(initiator, me, directory, (id) => agents[id]?.meta),
    [initiator, me, directory, agents],
  );
}
