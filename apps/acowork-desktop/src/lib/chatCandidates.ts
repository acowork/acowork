/**
 * The second level of the `@` (ADR-106 D7): the execution sessions this Chat may
 * address, read from the node that hosts the agent.
 *
 * The list is not a convenience filter the client could write itself - it is the
 * permission. Which sessions a human may continue in a chat is decided by the
 * Runtime's candidate rules (1:1 lists this chat's sessions plus the human's own
 * hand-started ones; a group lists only the speaker's own runs there; terminal
 * work never comes back). Asking the agent's own node is what keeps the picker
 * and the router agreeing, so a session the human can click is a session the
 * turn can actually be delivered to.
 */

import { getGatewayUrl } from "./config";
import { useAuthStore } from "../stores/authStore";

/** One addressable execution session. `status` is the candidate vocabulary
 *  (`working` / `idle` / `errored` / `closed`), not the Runtime's fine-grained
 *  one; `origin` says whether the session came out of this chat or a PM task. */
export interface ChatCandidate {
  session_id: string;
  label: string;
  status: string;
  origin: string;
  last_active_secs: number;
}

/** Which surface the picker is opening on. It selects the candidate rules, so
 *  it is taken from the panel that owns the chat - never guessed. */
export type CandidateSurface = "direct" | "group";

export async function fetchChatCandidates(
  instanceId: string,
  chatSessionId: string,
  surface: CandidateSurface,
  opts?: { signal?: AbortSignal },
): Promise<ChatCandidate[]> {
  if (!instanceId || !chatSessionId) return [];
  const token = useAuthStore.getState().accessToken;
  const query = new URLSearchParams({ chat_session_id: chatSessionId, surface });
  const resp = await fetch(
    `${getGatewayUrl()}/api/agents/${encodeURIComponent(instanceId)}/chat-candidates?${query}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {}, signal: opts?.signal },
  );
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`chat-candidates ${resp.status} ${text.slice(0, 200)}`);
  }
  const body = (await resp.json()) as { candidates?: ChatCandidate[] };
  return body.candidates ?? [];
}
