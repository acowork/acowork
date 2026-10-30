/**
 * Agent-file access — the execution surface's attachment bytes.
 *
 * Two address spaces hold "a file that hangs off a conversation", and they are
 * not interchangeable:
 *
 *   - chat-session attachments live in the user service
 *     (`/api/chat-sessions/{sid}/files/{id}`), owned by
 *     `stores/chatSessionStore.ts` + `lib/user-chat-api.ts`;
 *   - execution-surface uploads (`image_upload` / `file_upload` system entries
 *     in a Runtime transcript) live under the agent
 *     (`/api/agents/{agentId}/files/{documentId}`).
 *
 * This module owns the second one. It exists so the endpoint is spelled in
 * exactly one place: the thumbnail in `AttachmentChipRow` and the full-size
 * view used to build the same URL independently, and a copy is a drift risk
 * with no test able to see it.
 *
 * Plain `fetch`, **no token plumbing**: these paths are not under
 * `/api/auth/*`, so the app-wide interceptor ([authFetch.ts](authFetch.ts))
 * attaches the bearer and handles the 401 → refresh → replay. Threading tokens
 * through here would be a second, weaker copy of that.
 */
import { getGatewayUrl } from "./config";
import { saveUrlToDisk } from "./attachmentTransfer";

/** One agent file, addressed the only way the transcript row knows it. */
export interface AgentFileRef {
  agentId: string;
  documentId: string;
}

/** Not exported on purpose: a URL here is a *fetch* target, not an `<img src>`
 *  — a bare `src` never reaches the auth interceptor, so handing the string
 *  out is how the 401-blank-avatar bug gets written a second time. */
function agentFileUrl(agentId: string, documentId: string): string {
  return `${getGatewayUrl()}/api/agents/${encodeURIComponent(agentId)}/files/${encodeURIComponent(documentId)}`;
}

/**
 * Read one agent file as a `Blob`.
 *
 * Throws on a non-2xx response or a network failure — callers decide how the
 * failure becomes visible (a thumbnail degrades to a filename chip; a click
 * raises a toast). Returning `null` would push the same decision into every
 * call site.
 *
 * `signal` is not decoration: a transcript row is unmounted the moment it
 * scrolls out of the virtualized window, and an image download that nobody is
 * waiting for any more should stop rather than compete with the rows the user
 * is actually looking at.
 */
export async function fetchAgentFileBlob(
  agentId: string,
  documentId: string,
  opts?: { signal?: AbortSignal },
): Promise<Blob> {
  const resp = await fetch(agentFileUrl(agentId, documentId), { signal: opts?.signal });
  if (!resp.ok) throw new Error(`agent file → ${resp.status}`);
  return resp.blob();
}

/** Save one agent file to disk through the OS save dialog. */
export function saveAgentFile(
  ref: AgentFileRef,
  filename: string,
  onProgress?: (received: number) => void,
): Promise<boolean> {
  return saveUrlToDisk(agentFileUrl(ref.agentId, ref.documentId), filename, onProgress);
}
