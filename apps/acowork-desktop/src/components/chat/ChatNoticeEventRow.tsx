/**
 * ChatNoticeEventRow — the shared renderer for the LEGACY `system_notice` rows
 * a chat window may still contain.
 *
 * ADR-100 revision: the chat log records conversation, not progress. The
 * runtime no longer writes the dispatch / round-open milestones or the router
 * fallback receipt, so a notice row can only come from a transcript recorded
 * before the revision. Those rows are deliberately invisible now — history says
 * nothing about "queued" or "a round opened", and the live surface derives its
 * own waiting state from the transcript itself (see `chatTurnState`).
 *
 * What remains renderable: a receipt whose body is a genuine channel fact
 * (`delivery_failed`, `budget_exhausted`, …). `interaction_resolved` folds into
 * its card as the answered/closed receipt and paints nothing here.
 */

import { ChatEventText } from "./ChatEventText";
import { useTranslation } from "../../i18n/useTranslation";
import type { ChatMessageRow } from "../../stores/chatSessionStore";

/** Notice types the runtime stopped writing in the ADR-100 revision. They must
 *  not paint even when an old transcript still carries them. (`router_fallback`
 *  is written again — the degraded route posts a receipt the human can see —
 *  so it is NOT retired here.) */
const RETIRED_NOTICES = new Set(["turn_dispatched", "turn_started"]);

interface ChatNoticeEventRowProps {
  row: ChatMessageRow;
  /** Surface column gutter so the line lines up with the stream. */
  className?: string;
}

export function ChatNoticeEventRow({ row, className }: ChatNoticeEventRowProps) {
  const { t } = useTranslation();
  const noticeType = row.meta?.notice_type ?? "";

  // The closure is shown on its card as an answered/closed receipt.
  if (noticeType === "interaction_resolved") return null;
  // Retired milestones: history only.
  if (RETIRED_NOTICES.has(noticeType)) return null;
  // The router-fallback receipt is keyed by notice_type, not body: the
  // runtime writes a bodyless row and every surface owns the wording.
  // Both branches below are channel receipts — a fact about the channel, not
  // about any one row — so they wear the transcript's `turn` tier of the shared
  // event-text grammar (see `ChatEventText`): centered and icon-less.
  if (noticeType === "router_fallback") {
    return (
      <ChatEventText
        anchor="turn"
        text={t("chatView.routerFallback")}
        className={className}
      />
    );
  }

  const text = row.body || row.meta?.status || "";
  if (!text) return null;
  return (
    <ChatEventText
      anchor="turn"
      text={text === "routerFallback" ? t("chatView.routerFallback") : text}
      className={className}
    />
  );
}

/** Bound a raw trigger body used as a task-name fallback so a long first
 *  message cannot wrap a status line. Collapses whitespace and clips with an
 *  ellipsis, mirroring how the pending-queue preview bounds its text. */
export function truncateTaskName(body: string | undefined): string {
  const collapsed = (body ?? "").trim().replace(/\s+/g, " ");
  if (!collapsed) return "";
  return collapsed.length > 40 ? `${collapsed.slice(0, 40)}…` : collapsed;
}
