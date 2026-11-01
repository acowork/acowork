/**
 * ChatUsageIcon — the per-chat token usage button on the Chat surface
 * composers (1:1 `AgentChatView` + `GroupChatPanel`).
 *
 * Why it is not the execution surface's `ContextUsageIcon`: a Chat
 * session has no context window (ADR-099 — the router is stateless), so
 * there is no usage *percentage* to ring. What a chat DOES have is the
 * token ledger each agent participant keeps for it
 * (`chat_token_usage`, plan §4.6 phase two) — cumulative spend
 * attributed to this conversation. The popover renders exactly that:
 * cross-agent totals, per-model share, and (multi-agent groups only)
 * the per-agent split.
 *
 * Data is pulled on mount and re-pulled whenever the popover opens —
 * no push channel exists for ledger rows, and the numbers are
 * lifetime-cumulative, so hover-time freshness is the right budget.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { X } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";
import { fetchChatUsage, type ChatUsageSummary } from "../../lib/chatUsage";
import { computeTokenSegments, formatTokenStatCost } from "../../lib/tokenStats";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";

/** Same precision contract as `RightPanel.formatTokenCount` /
 *  `ContextUsageIcon.formatTokens`: 2 decimals for M, 1 for K. */
function formatTokens(n: number | undefined): string {
  if (n == null) return "\u2014";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-center justify-between py-0.5">
      <span className="text-text-tertiary">{label}</span>
      <span className="font-mono text-text-secondary">{value}</span>
    </div>
  );
}

export function ChatUsageIcon({
  agentIds,
  chatSessionId,
  resolveName,
}: {
  agentIds: string[];
  chatSessionId: string;
  /** Agent display names for the per-agent split (group rosters). */
  resolveName?: (id: string) => string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [summary, setSummary] = useState<ChatUsageSummary | null>(null);
  const [loading, setLoading] = useState(false);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Stable fan-out key: re-pull when the chat switches or the roster's
  // agent set changes (sorted join — order alone must not re-trigger).
  const rosterKey = [...agentIds].sort().join(",");

  const load = useCallback(() => {
    const ids = rosterKey ? rosterKey.split(",") : [];
    if (ids.length === 0) return;
    setLoading(true);
    void fetchChatUsage(ids, chatSessionId)
      .then((s) => setSummary(s))
      .finally(() => setLoading(false));
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [rosterKey, chatSessionId]);

  useEffect(() => {
    load();
  }, [load]);

  useEffect(() => {
    return () => {
      if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    };
  }, []);

  const handleMouseEnter = () => {
    if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    setOpen(true);
    // Re-pull on every open: the ledger grows while the chat lives, and
    // hover is the moment the user is about to read the numbers.
    load();
  };
  const handleMouseLeave = () => {
    closeTimerRef.current = setTimeout(() => setOpen(false), 150);
  };

  // Nothing to attribute: a chat with no agent participants (human-only
  // DMs / groups) records no router spend, ever — and no session id yet
  // means the surface has nothing open. Hide, do not disable.
  if (agentIds.length === 0 || !chatSessionId) return null;

  const segments = summary ? computeTokenSegments(summary.modelItems) : [];
  const multiAgent = (summary?.perAgent.length ?? 0) > 1;

  return (
    <div
      className="relative"
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
    >
      <button
        type="button"
        className={cn(toolbarIconButton, loading && "opacity-50")}
        aria-label={t("chatUsage.ariaLabel")}
      >
        {/* Gauge glyph — deliberately not the exec surface's progress
            ring: there is no percentage of anything here. */}
        <svg
          width={16}
          height={16}
          viewBox="0 0 24 24"
          fill="none"
          stroke="currentColor"
          strokeWidth={2}
          strokeLinecap="round"
          strokeLinejoin="round"
        >
          <path d="M12 14l3.5-3.5" />
          <path d="M20.3 18a8.32 8.32 0 0 0 .7-3 9 9 0 1 0-17 0 8.32 8.32 0 0 0 .7 3" />
          <path d="M12 18a1 1 0 1 0 0-.01" />
        </svg>
      </button>

      {open && (
        <div
          role="dialog"
          aria-label={t("chatUsage.title")}
          // Sibling popover grammar (ModelMenu / ContextUsageIcon):
          // bottom-right anchored, w-72, rounded-md, bg-modal-surface.
          className="absolute bottom-full right-0 z-50 mb-2 w-72 max-h-[calc(100vh-32px)] select-none overflow-y-auto overscroll-contain rounded-md border border-border-outer bg-modal-surface text-text-secondary shadow-lg"
        >
          <div className="flex items-center justify-between px-3 pt-2.5">
            <h2 className="text-sm font-normal text-text-secondary">
              {t("chatUsage.title")}
            </h2>
            <button
              type="button"
              onClick={() => setOpen(false)}
              aria-label={t("chatUsage.close")}
              title={t("chatUsage.close")}
              className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-700 dark:hover:bg-zinc-700/50 dark:hover:text-zinc-100"
            >
              <X size={14} strokeWidth={2.25} />
            </button>
          </div>

          <div className="px-3 pb-3 pt-1 text-xs">
            {summary && summary.llmCallCount > 0 ? (
              <>
                <Row label={t("chatUsage.llmCalls")} value={String(summary.llmCallCount)} />
                <Row label={t("chatUsage.totalInput")} value={formatTokens(summary.totalInput)} />
                <Row label={t("chatUsage.totalOutput")} value={formatTokens(summary.totalOutput)} />
                {(summary.totalCacheRead > 0) && (
                  <Row
                    label={t("chatUsage.cacheRead")}
                    value={formatTokens(summary.totalCacheRead)}
                  />
                )}
                {summary.totalCacheWrite > 0 && (
                  <Row
                    label={t("chatUsage.cacheWrite")}
                    value={formatTokens(summary.totalCacheWrite)}
                  />
                )}
                <Row
                  label={t("chatUsage.estCost")}
                  value={formatTokenStatCost(summary.totalCost)}
                />
                {segments.length > 0 && (
                  <>
                    <div className="mt-2 border-t border-border-divider pt-2">
                      <div className="mb-1 text-text-tertiary">
                        {t("chatUsage.byModel")}
                      </div>
                      {segments.map((seg) => (
                        <div
                          key={`${seg.provider}|${seg.model}`}
                          className="flex items-center justify-between py-0.5"
                        >
                          <span className="flex min-w-0 items-center gap-1.5">
                            <span
                              className="h-2 w-2 shrink-0 rounded-full"
                              style={{ backgroundColor: seg.color }}
                            />
                            <span className="truncate text-text-secondary">
                              {seg.isUnknown
                                ? t("chatUsage.unknownModel")
                                : `${seg.provider !== "unknown" ? `${seg.provider}/` : ""}${seg.model}`}
                            </span>
                          </span>
                          <span className="ml-2 shrink-0 font-mono text-text-tertiary">
                            {formatTokens(seg.value)} · {seg.sharePct}%
                          </span>
                        </div>
                      ))}
                    </div>
                  </>
                )}
                {multiAgent && (
                  <div className="mt-2 border-t border-border-divider pt-2">
                    <div className="mb-1 text-text-tertiary">
                      {t("chatUsage.byAgent")}
                    </div>
                    {summary.perAgent.map(({ agentId, row }) => (
                      <div
                        key={agentId}
                        className="flex items-center justify-between py-0.5"
                      >
                        <span className="truncate text-text-secondary">
                          {resolveName?.(agentId) || agentId.slice(0, 8)}
                        </span>
                        <span className="ml-2 shrink-0 font-mono text-text-tertiary">
                          {formatTokens(
                            (row?.tokens.total_input ?? 0) +
                              (row?.tokens.total_output ?? 0),
                          )}{" "}
                          · {row?.llm_call_count ?? 0}
                        </span>
                      </div>
                    ))}
                  </div>
                )}
              </>
            ) : (
              <div className="py-2 text-center text-text-tertiary italic">
                {t("chatUsage.noData")}
              </div>
            )}
          </div>
        </div>
      )}
    </div>
  );
}
