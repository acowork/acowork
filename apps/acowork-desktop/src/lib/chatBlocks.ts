/**
 * The Chat surface's execution-block read (ADR-106 D3/D5/D9).
 *
 * A chat transcript holds one bodyless `execution_round` row per round of work
 * (D3). What the round actually did — the human's row, the agent's answer, the
 * card it is parked on, how it ended — lives in the EXECUTION session and is
 * served by `GET /api/chat-blocks/{chatId}?refs=`, which composes this chat's
 * projection cache with a live read from the hosting Runtime.
 *
 * The two rules this module exists to keep:
 * - a ref is the row's own coordinate triple, never a lookup the client invents
 *   (a missing `agent_session_id` means there is no block to read, D3);
 * - `pending` is the only status that may be re-read: a terminal projection is
 *   immutable and already cached server-side (D5), so polling it would be
 *   re-fetching history as if it were live.
 */

import { getGatewayUrl } from "./config";
import { useAuthStore } from "../stores/authStore";

/** The card a round is parked on (ADR-106 D8): what the round is waiting for,
 *  and what the human can answer with. Rendered inside the block, never as a
 *  chat row. */
export interface BlockCard {
  /** `approval` | `ask_question` | `pause`. */
  kind: string;
  /** The coordinate the answer resolves against. */
  request_id: string;
  /** The question, in the agent's own words. */
  prompt?: string;
  /** Labels the human may pick; empty for a free-text answer. */
  options?: string[];
}

/** One round's projected content, verbatim from the execution session. */
export interface RoundProjection {
  round_id: string;
  /** The Chat this round's session is bound to — checked server-side (D10). */
  chat_session_id?: string;
  user?: { body: string; ts: string } | null;
  answer?: { body: string; ts: string } | null;
  card?: BlockCard | null;
  terminal: boolean;
  /** `in_progress` / `completed` / `aborted` / `failed` / `waiting` /
   *  `interrupted` / `unknown`. */
  status: string;
}

/** One entry of the block read's answer, in request order. */
export interface ChatBlockEntry {
  ref: string;
  agent_instance_id: string;
  agent_session_id: string;
  round_id: string;
  /** `ok` renders the block; `pending` renders it live and may be re-read;
   *  `not_found` is an explicit hole; `unreachable` offers a retry (D9). */
  status: "ok" | "pending" | "not_found" | "unreachable";
  source?: "cache" | "runtime";
  projection?: RoundProjection;
  detail?: string;
}

/** The row fields a block ref is built from. `ChatMessageRow.meta`'s relevant
 *  subset — kept structural so this module does not own the transcript type. */
export interface BlockRefParts {
  agent_instance_id?: string;
  agent_session_id?: string;
  round_id?: string;
}

/** `instance|session|round`, the Gateway's cache and response key.
 *  `null` when the row names no resolvable round: a reference missing any of its
 *  three coordinates addresses nothing, and inventing a fourth would put a block
 *  under a row that has no run behind it (D3). */
export function blockRefOf(meta?: BlockRefParts): string | null {
  const instance = meta?.agent_instance_id?.trim() ?? "";
  const session = meta?.agent_session_id?.trim() ?? "";
  const round = meta?.round_id?.trim() ?? "";
  if (!instance || !session || !round) return null;
  return `${instance}|${session}|${round}`;
}

/** One authenticated batched read. Refs the caller already holds are trimmed
 *  here so a re-render mid-flight cannot re-request a block that just landed. */
export async function fetchChatBlocks(
  chatId: string,
  refs: string[],
  opts?: { signal?: AbortSignal },
): Promise<ChatBlockEntry[]> {
  if (refs.length === 0) return [];
  const token = useAuthStore.getState().accessToken;
  const query = refs.map(encodeURIComponent).join(",");
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-blocks/${encodeURIComponent(chatId)}?refs=${query}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {}, signal: opts?.signal },
  );
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`chat-blocks ${resp.status} ${text.slice(0, 200)}`);
  }
  const body = (await resp.json()) as { blocks?: ChatBlockEntry[] };
  return body.blocks ?? [];
}
