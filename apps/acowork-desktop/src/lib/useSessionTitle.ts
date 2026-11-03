/**
 * useSessionTitle — resolve a session's display name for a chat surface.
 *
 * Reads the SAME two sources the tab strip (`SessionTabBar`) does, in the same
 * order: the live per-session overlay `chatStore.agentStates[agentId]
 * .sessionStates[sid].title` (fed by the MQTT `session_config` push, so it is
 * the freshest and covers a just-routed exec session that has no list row yet),
 * then the `agentStore` `sessions[]` snapshot for sessions never opened in this
 * window. Returns `undefined` while neither carries a name (a brand-new turn
 * before its optimistic seed lands).
 *
 * Every lookup is optional-chained so an empty store — or a partial mock in a
 * test — yields `undefined` instead of throwing.
 */
import { useChatStore } from "../stores/chatStore";
import { useAgentStore } from "../stores/agentStore";

export function useSessionTitle(
  agentId: string,
  sessionId: string | null | undefined,
): string | undefined {
  // Both hooks run unconditionally; the guard lives inside each selector.
  const live = useChatStore((s) =>
    sessionId ? s.agentStates?.[agentId]?.sessionStates?.[sessionId]?.title : undefined,
  );
  const snapshot = useAgentStore((s) =>
    sessionId
      ? s.agents?.[agentId]?.sessions?.find((x) => x.session_id === sessionId)?.title
      : undefined,
  );
  const name = (live || snapshot || "").trim();
  return name || undefined;
}
