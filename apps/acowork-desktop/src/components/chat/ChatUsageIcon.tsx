/**
 * ChatUsageIcon — the per-chat usage button on the Chat surface
 * composers (1:1 `AgentChatView` + `GroupChatPanel`).
 *
 * **Layout: the execution surface's `ContextUsageIcon`, verbatim.** The
 * popover is a deliberate copy of that surface's grammar — the big
 * headline block (percent + `Used X / Y` + pencil), the cache-hit-rate
 * block, the same popover shell (`w-72`, `bg-modal-surface`, bottom-right
 * anchored). Two things are intentionally DIFFERENT, because the Chat
 * session has no such facts:
 *
 *   - **No per-category breakdown.** The five-category split
 *     (system prompt / tools / messages / connectors / skills) comes from
 *     an execution session's assembled context. A Chat session's router
 *     turn reads a projected history window (ADR-099 / design 29), not a
 *     session context — there is no category split to show, so the
 *     progress bar and the percentage list are omitted rather than
 *     faked.
 *   - **No cost.** The exec surface shows an estimated cost; the Chat
 *     ledger has a cost bucket too, but the user asked for it to stay
 *     out of this menu (the agent-level donut remains the place where
 *     spend is discussed).
 *
 * **What the headline means.** The Chat Session Router reads at most
 * `chat_router.history_max_tokens` tokens of history per turn (default
 * 8,000). The headline therefore reads:
 *
 *   - the big number = `history_tokens / window` — how much of the
 *     router's history budget the most recent turn actually consumed.
 *     This is the Chat-side analogue of the exec surface's
 *     `used / window`.
 *   - the small `Used this turn X / Y` = the same two numbers in absolute
 *     tokens. "this turn" is stated because the popover carries a SECOND
 *     measure that is cumulative — see below.
 *   - the pencil edits `Y` — the SAME field the Agent Setup panel's
 *     "Chat Session Router" card edits
 *     (`chat_router.history_max_tokens`, see `lib/chatRouterConfig.ts`).
 *     One value, two surfaces, one wire field.
 *
 * **Two different measures live in this one card** (the reason each section
 * header names its own measure): the headline is a PER-TURN snapshot (the
 * latest router turn's projected history window, `history_tokens`), while the
 * `By model` / `By agent` rows are WHOLE-CHAT cumulative `total_input +
 * total_output`. A chat with one turn shows the same tokens in both; after
 * that the cumulative figure keeps growing past the per-turn one — a mismatch
 * by design, not a miscomputation. The model share percentage only renders
 * when more than one model appears: a lone `100%` carries no information and
 * reads like it belongs to the headline's percent.
 *
 * **One type scale, four steps** (borrowed wholesale from the exec surface so
 * the two popovers cannot drift): `text-sm` for the popover title and every
 * section header — the measure goes in the label, the exec surface's
 * "Cache hit rate (cumulative)" grammar — `text-xs` for rows and their
 * sub-lines, `text-10` for editor helpers and errors, `text-11` for the
 * editor's buttons, plus the shared `clamp()` headline figure. Do not add a
 * size outside that set; a section header and its column caption at two
 * different sizes was exactly the "why is `By model` bigger than `chat total`"
 * report.
 *
 * **Data.** The token ledger each agent participant keeps for the chat
 * (`chat_token_usage`, plan §4.6 phase two) — cumulative spend attributed
 * to this conversation. Pulled on mount and re-pulled whenever the
 * popover opens: no push channel exists for ledger rows, and the numbers
 * are lifetime-cumulative, so hover-time freshness is the right budget.
 *
 * **`history_tokens` is a snapshot, not a running total.** The column was
 * added after the ledger had already been in service (schema v10), so a
 * row written before it carries `NULL` until that chat's next router turn
 * — and `NULL` renders as `—`, never as a fabricated `0%`. Aggregate
 * counters (input/output/cache) do NOT have this gap, so a legacy row can
 * show a cache block and a dash side by side; that is the honest reading,
 * not a rendering bug.
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { Pencil, X } from "lucide-react";
import { CircularProgressIcon } from "./CircularProgressIcon";
import { useTranslation } from "../../i18n/useTranslation";
import { fetchChatUsage, type ChatUsageSummary } from "../../lib/chatUsage";
import { computeTokenSegments } from "../../lib/tokenStats";
import { computeCacheHitStats, formatCacheHitRate } from "../../lib/cacheHitRate";
import { formatDetailedPercent } from "../../lib/contextUsageBreakdown";
import {
  fetchRouterWindow,
  ROUTER_WINDOW_CEILING,
  ROUTER_WINDOW_DEFAULT,
  ROUTER_WINDOW_FLOOR,
  ROUTER_WINDOW_PRESETS,
  saveRouterWindow,
} from "../../lib/chatRouterConfig";
import { cn } from "../../lib/utils";
import { toolbarIconButton } from "../../lib/ui-styles";

/** Same precision contract as `RightPanel.formatTokenCount` /
 *  `ContextUsageIcon.formatTokens`: 2 decimals for M, 1 for K. */
function formatTokens(n: number | undefined): string {
  if (n == null) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return String(n);
}

export function ChatUsageIcon({
  agentIds,
  chatSessionId,
  resolveName,
  /** The single agent whose router config this menu edits. Omitted on
   *  multi-agent surfaces (a group's participants each own their own
   *  router config, so "the" window would be ambiguous) — the menu then
   *  shows the budget read-only. */
  editableAgentId,
}: {
  agentIds: string[];
  chatSessionId: string;
  /** Agent display names for the per-agent split (group rosters). */
  resolveName?: (id: string) => string;
  /** Agent whose `chat_router.history_max_tokens` the pencil edits. */
  editableAgentId?: string;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [summary, setSummary] = useState<ChatUsageSummary | null>(null);
  const [loading, setLoading] = useState(false);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  // Stable fan-out key: re-pull when the chat switches or the roster's
  // agent set changes (sorted join — order alone must not re-trigger).
  const rosterKey = [...agentIds].sort().join(",");

  // ── The router's history budget (the popover's denominator) ───────
  // `null` = no override on disk → the runtime default applies. Read
  // once per popover open: the value changes only through a PUT, and
  // this component is the only chat-side writer.
  const [windowTokens, setWindowTokens] = useState<number | null>(null);
  const [editingWindow, setEditingWindow] = useState(false);
  const [windowDraft, setWindowDraft] = useState<string>("");
  const [windowError, setWindowError] = useState<string | null>(null);

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
    if (editableAgentId) {
      void fetchRouterWindow(editableAgentId).then(setWindowTokens);
    }
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

  // The effective budget: the on-disk override, else the runtime default.
  const effectiveWindow = windowTokens ?? ROUTER_WINDOW_DEFAULT;
  // The honest "used" figure: how many tokens the router's projected
  // history window actually cost on the most recent turn.
  //
  // This is NOT `last_input` — the provider-reported prompt count also
  // carries the system prompt and the tool schemas, so it routinely
  // exceeds the history budget and would render as >100%. The ledger's
  // `history_tokens` is the only number comparable with the budget.
  //
  // `null` = never measured (a row written before the column existed, or a
  // turn that projected no history). The headline then shows a dash
  // rather than a fabricated 0% — "宁可 miss 也不估计" (ADR-027).
  const usedTokens = summary?.historyTokens ?? null;
  const usagePercent =
    usedTokens != null && effectiveWindow > 0
      ? Math.min(100, (usedTokens / effectiveWindow) * 100)
      : null;

  // ADR-066 §6 cache-hit ratio, cumulative window. The Chat ledger
  // records the same cache buckets the execution session does, so the
  // SAME helper computes the ratio — the denominator choice (Anthropic
  // vs OpenAI family) stays in one place.
  const cacheStats = computeCacheHitStats(
    segments[0]?.provider ?? null,
    {
      total_cache_read_tokens: summary?.totalCacheRead ?? 0,
      total_cache_write_tokens: summary?.totalCacheWrite ?? 0,
      total_input_tokens: summary?.totalInput ?? 0,
    },
    "cumulative",
  );
  const cacheHitRateLabel = formatCacheHitRate(cacheStats.ratio);
  // Same gate as the exec surface (`hasCacheData(..., "cumulative")`):
  // the block appears only when the provider actually reported cache
  // accounting. A provider that reports none keeps the row hidden
  // rather than showing a meaningless "—%".
  const hasCache = summary != null && (summary.totalCacheRead > 0 || summary.totalCacheWrite > 0);

  const openWindowEditor = () => {
    // Existing override → prefill in K units; no override → leave empty
    // with the placeholder showing the effective default (same rule as
    // the exec surface's editor: the editor never introduces a second
    // data source).
    setWindowDraft(windowTokens != null ? String(windowTokens / 1000) : "");
    setWindowError(null);
    setEditingWindow(true);
  };

  const saveWindow = async () => {
    if (!editableAgentId) return;
    const trimmed = windowDraft.trim();
    // Empty input = don't send the field = don't write an override (the
    // inherited default must never be pinned as an override).
    if (trimmed === "") {
      const ok = await saveRouterWindow(editableAgentId, null);
      if (!ok) {
        setWindowError(
          t("agentSetup.saveFailed", {
            field: "chat_router_history_max_tokens",
            status: "network",
          }),
        );
        return;
      }
      setWindowError(null);
      setWindowTokens(null);
      setEditingWindow(false);
      return;
    }
    const abs = Math.round(parseFloat(trimmed) * 1000);
    if (
      !Number.isFinite(abs) ||
      abs < ROUTER_WINDOW_FLOOR ||
      abs > ROUTER_WINDOW_CEILING
    ) {
      setWindowError(
        t("contextUsage.windowRangeError", {
          floor: ROUTER_WINDOW_FLOOR / 1000,
          ceiling: ROUTER_WINDOW_CEILING / 1000,
        }),
      );
      return;
    }
    const ok = await saveRouterWindow(editableAgentId, abs);
    if (!ok) {
      setWindowError(
        t("agentSetup.saveFailed", {
          field: "chat_router_history_max_tokens",
          status: "network",
        }),
      );
      return;
    }
    setWindowError(null);
    setWindowTokens(abs);
    setEditingWindow(false);
  };

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
        {/* The SAME glyph the execution composer uses — one control
            family across both surfaces. A `null` percentage draws an
            empty ring rather than hiding the button: the affordance
            (open the menu) must still be there before the first turn
            has been measured. */}
        <CircularProgressIcon usagePercent={usagePercent ?? 0} />
      </button>

      {open && (
        <div
          role="dialog"
          aria-label={t("chatUsage.title")}
          // Sibling popover grammar (ContextUsageIcon): bottom-right
          // anchored, w-72, rounded-md, bg-modal-surface.
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

          {/* ── Headline block (exec surface's layout, verbatim) ────── */}
          <div className="px-3 pt-2">
            <div className="flex items-center gap-2 whitespace-nowrap">
              <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-medium leading-none tracking-[-0.01em] text-text-secondary tabular-nums ">
                {usagePercent != null ? `${formatDetailedPercent(usagePercent)}%` : "—"}
              </span>
              <span
                className="text-xs text-text-tertiary"
                title={
                  usedTokens == null
                    ? t("chatUsage.usedNotMeasured")
                    : undefined
                }
              >
                {/* "this turn" is load-bearing: the headline's `used` is the
                    LATEST router turn's history-window occupancy, while the
                    by-model rows below are whole-chat cumulative. Two
                    different measures in one card, so each one says which
                    it is. */}
                {t("chatUsage.usedThisTurn")} {" "}
                <span className="font-mono text-text-secondary ">
                  {formatTokens(usedTokens ?? undefined)}
                </span>
                <span className="text-text-tertiary "> / </span>
                <span className="font-mono text-text-secondary ">
                  {formatTokens(effectiveWindow)}
                </span>
              </span>
              {/* The same field the Agent Setup panel's Chat Session
                  Router card edits — one value, two surfaces. */}
              {editableAgentId && (
                <button
                  type="button"
                  onClick={openWindowEditor}
                  aria-label={t("contextUsage.editWindow")}
                  title={t("contextUsage.editWindow")}
                  className="ml-1 rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-700  dark:hover:bg-zinc-700/50 dark:hover:text-zinc-100"
                >
                  <Pencil size={11} strokeWidth={2.25} />
                </button>
              )}
            </div>

            {/* History-budget editor (inline, same popover). */}
            {editingWindow && (
              <div className="mt-2.5 rounded-md border border-border-outer p-2">
                <div className="flex items-center gap-1.5">
                  <input
                    type="number"
                    min={ROUTER_WINDOW_FLOOR / 1000}
                    max={ROUTER_WINDOW_CEILING / 1000}
                    step="any"
                    value={windowDraft}
                    onChange={(e) => {
                      setWindowDraft(e.target.value);
                      setWindowError(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") void saveWindow();
                      if (e.key === "Escape") setEditingWindow(false);
                    }}
                    placeholder={String(ROUTER_WINDOW_DEFAULT / 1000)}
                    aria-label={t("contextUsage.editWindow")}
                    className="w-full min-w-0 rounded border border-input-border bg-input-bg px-1.5 py-1 text-xs tabular-nums text-text-secondary outline-none focus:border-indigo-400 "
                  />
                  <span className="shrink-0 text-10 text-text-tertiary">
                    {t("contextUsage.windowUnitK")}
                  </span>
                </div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {ROUTER_WINDOW_PRESETS.map((preset) => (
                    <button
                      key={preset}
                      type="button"
                      onClick={() => {
                        setWindowDraft(String(preset / 1000));
                        setWindowError(null);
                      }}
                      className={cn(
                        "rounded border border-zinc-200 px-1.5 py-0.5 text-10 text-text-secondary transition-colors",
                        "hover:border-indigo-300 hover:text-indigo-600",
                        "dark:border-zinc-700  dark:hover:border-indigo-500/50 dark:hover:text-indigo-300",
                      )}
                    >
                      {preset / 1000}K
                    </button>
                  ))}
                </div>
                {windowError != null && (
                  <p className="mt-1.5 text-10 leading-snug text-red-600 dark:text-red-400">
                    {windowError}
                  </p>
                )}
                <div className="mt-2 flex items-center gap-1.5">
                  <button
                    type="button"
                    onClick={() => void saveWindow()}
                    className="rounded bg-accent-strong px-2 py-1 text-11 font-medium text-white transition-[filter] hover:brightness-90"
                  >
                    {t("contextUsage.save")}
                  </button>
                  <button
                    type="button"
                    onClick={() => setEditingWindow(false)}
                    className="rounded px-2 py-1 text-11 text-text-tertiary transition-colors hover:bg-zinc-100  dark:hover:bg-zinc-700/50"
                  >
                    {t("contextUsage.cancel")}
                  </button>
                </div>
              </div>
            )}

            </div>

          {/* ── Cache hit rate block (exec surface's layout) ───────── */}
          {hasCache && (
            <>
              <div className="border-t border-border-divider" />
              <div className="px-3 pt-2 pb-3">
                <div className="text-sm font-normal text-text-secondary ">
                  {t("contextUsage.cacheHitLabel")}
                </div>
                <div className="mt-1 flex items-baseline gap-2 whitespace-nowrap">
                  <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-medium leading-none tracking-[-0.01em] tabular-nums text-text-secondary ">
                    {cacheHitRateLabel ?? "—"}
                  </span>
                  <span className="text-xs text-text-tertiary ">
                    {t("contextUsage.cached")}{" "}
                    <span className="font-mono text-text-secondary ">
                      {formatTokens(cacheStats.numerator)}
                    </span>
                    <span className="text-text-tertiary "> / </span>
                    <span className="font-mono text-text-secondary ">
                      {formatTokens(cacheStats.denominator)}
                    </span>
                  </span>
                </div>
              </div>
            </>
          )}

          <div className="px-3 pb-3 pt-1 text-xs">
            {summary && summary.llmCallCount > 0 ? (
              <>
                {/* The cumulative counters (LLM calls / total input /
                    total output / cache read+write) are deliberately NOT
                    rendered: the headline's `Used X / Y` and the cache
                    block above already carry the two figures a reader
                    acts on, and the rest is ledger bookkeeping. The
                    per-model split below is kept because it is the only
                    place the serving model appears. */}
                {segments.length > 0 && (
                  <>
                    <div className="mt-2 border-t border-border-divider pt-2">
                      {/* Section header = the popover's ONE header step
                          (`text-sm`, the cache block's size), with the
                          measure in the label itself — the exec surface's
                          grammar ("Cache hit rate (cumulative)"). A separate
                          smaller column caption put two font sizes on one
                          line and read as an accident. */}
                      <div className="mb-1 text-sm font-normal text-text-secondary">
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
                            {formatTokens(seg.value)}
                            {/* A share is only information when there is
                                another model to compare against; a lone
                                "100%" reads like it belongs to the
                                headline's percent and is pure noise. */}
                            {segments.length > 1 && ` · ${seg.sharePct}%`}
                          </span>
                        </div>
                      ))}
                    </div>
                  </>
                )}
                {multiAgent && (
                  <div className="mt-2 border-t border-border-divider pt-2">
                    <div className="mb-1 text-sm font-normal text-text-secondary">
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
                        {/* Tokens only. The call count used to ride along
                            as a bare `· 4` — a second unlabelled number,
                            and one the docstring above already promised
                            not to render (ledger bookkeeping). */}
                        <span className="ml-2 shrink-0 font-mono text-text-tertiary">
                          {formatTokens(
                            (row?.tokens.total_input ?? 0) +
                              (row?.tokens.total_output ?? 0),
                          )}
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
