/**
 * In-conversation history search client (WeChat-style 文本 / 图片 / 文件).
 *
 * Talks to `GET /api/chat-sessions/{sid}/search` (ADR-097): the backend reads
 * only the per-session disposable `search.sqlite` sidecar and answers with a
 * cursor-paged hit list. Requests are identity-bearing — the Gateway user
 * proxy verifies the bearer token and injects the caller, so a non-participant
 * gets a hidden 404. Mirrors the plain-fetch shape of `chatSessionStore`'s own
 * HTTP layer (same Gateway base, same bearer handling).
 */

import { getGatewayUrl } from "./config";
import { useAuthStore } from "../stores/authStore";
import type { ChatAttachmentMeta } from "../stores/chatSessionStore";

/** The three browse categories, matching the backend `category` param. */
export type HistoryCategory = "text" | "image" | "file";

/** One search hit — mirrors `search_index::SearchHit`. `line_pos` is the
 *  physical-line coordinate the frontend converts into a paging offset to
 *  locate (scroll to + highlight) the message in the conversation window. */
export interface HistoryHit {
  seq: number;
  line_pos: number;
  ts: number;
  from: string;
  kind: string;
  body: string;
  attachments: ChatAttachmentMeta[];
}

/** Mirrors `chat_sessions_api::SearchPage`. */
export interface HistoryPage {
  hits: HistoryHit[];
  total_lines: number;
  /** Cursor to continue toward older rows; `null` at the oldest end. */
  next_before_line: number | null;
}

/** One page of in-conversation history search. `beforeLine` is the
 *  `next_before_line` cursor from the previous page (omit for the newest). */
export async function searchChatHistory(
  chatSessionId: string,
  category: HistoryCategory,
  q: string,
  beforeLine?: number | null,
  limit = 50,
): Promise<HistoryPage> {
  const token = useAuthStore.getState().accessToken;
  const params = new URLSearchParams({ category, q, limit: String(limit) });
  if (beforeLine != null) params.set("before_line", String(beforeLine));
  const resp = await fetch(
    `${getGatewayUrl()}/api/chat-sessions/${encodeURIComponent(chatSessionId)}/search?${params}`,
    { headers: token ? { Authorization: `Bearer ${token}` } : {} },
  );
  if (!resp.ok) {
    const text = await resp.text().catch(() => "");
    throw new Error(`search ${chatSessionId} → ${resp.status} ${text.slice(0, 200)}`);
  }
  return resp.json() as Promise<HistoryPage>;
}
