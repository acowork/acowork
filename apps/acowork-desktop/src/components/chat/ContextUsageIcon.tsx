import { useState, useRef, useEffect, useCallback } from "react";
import { Minimize2, Pencil, Square, X } from "lucide-react";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useTranslation } from "../../i18n/useTranslation";
import { fetchProviderModels } from "../../lib/gateway-api";
import {
  computeContextUsageBreakdown,
  formatDetailedPercent,
  type ContextUsageCategoryKey,
} from "../../lib/contextUsageBreakdown";
import { cn } from "../../lib/utils";
import { getProcessingPhase } from "../../lib/types";
import { computeCacheHitStats, formatCacheHitRate, hasCacheData } from "../../lib/cacheHitRate";
import { COMPRESS_SUMMARY } from "../../lib/session-control";
import { useSessionReadOnly } from "../../lib/session-write-access";
import { toolbarIconButton } from "../../lib/ui-styles";

// ADR-074: the editor mirrors the backend `is_valid_context_window`
// bounds (CONTEXT_WINDOW_FLOOR..=CEILING) so out-of-range input is
// caught client-side before the PUT 400. Input is entered in K units
// (×1000 → absolute tokens), matching the UI's token formatting.
const WINDOW_FLOOR = 8_192;
const WINDOW_CEILING = 4_194_304;
const WINDOW_PRESETS = [200_000, 250_000, 300_000, 500_000];

const CATEGORY_META: Record<ContextUsageCategoryKey, { labelKey: string; color: string }> = {
  system: { labelKey: "contextUsage.categories.systemPrompt", color: "#6366f1" },
  tools: { labelKey: "contextUsage.categories.tools", color: "#14b8a6" },
  messages: { labelKey: "contextUsage.categories.messages", color: "#f59e0b" },
  connectors: { labelKey: "contextUsage.categories.connectors", color: "#a855f7" },
  skills: { labelKey: "contextUsage.categories.skills", color: "#ec4899" },
};

/** Circular progress ring showing context usage percentage.
 *  Starts from bottom (6 o'clock), goes clockwise.
 *  16x16 SVG to match adjacent send button icon size. */
function CircularProgressIcon({ usagePercent }: { usagePercent: number }) {
  const size = 16;
  const strokeWidth = 2;
  const radius = (size - strokeWidth) / 2;
  const center = size / 2;
  const circumference = 2 * Math.PI * radius;
  const offset = circumference * (1 - usagePercent / 100);

  const fillColor = "var(--color-text-secondary, hsl(240 3.7% 46.9%))";
  const emptyColor = "var(--shimmer-mid, #e8e8ec)";

  return (
    <svg
      width={size}
      height={size}
      viewBox={`0 0 ${size} ${size}`}
      fill="none"
      xmlns="http://www.w3.org/2000/svg"
    >
      {/* Background ring (total capacity) */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={emptyColor}
        strokeWidth={strokeWidth}
        opacity={0.35}
      />
      {/* Progress ring (used amount) — starts from bottom, goes clockwise */}
      <circle
        cx={center}
        cy={center}
        r={radius}
        stroke={fillColor}
        strokeWidth={strokeWidth}
        strokeLinecap="round"
        strokeDasharray={circumference}
        strokeDashoffset={offset}
        transform={`rotate(90 ${center} ${center})`}
        style={{ transition: "stroke-dashoffset 0.3s ease" }}
      />
    </svg>
  );
}

export function ContextUsageIcon({ agentId, sessionId }: { agentId: string; sessionId: string }) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const popoverRef = useRef<HTMLDivElement>(null);
  const closeTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);

  const contextUsage = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.contextUsage ?? null);
  const isCompacting = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.isCompacting ?? false);
  const sessionStatus = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.sessionStatus ?? null);
  // ADR-066: prompt-cache hit ratio is provider-specific (denominator
  // differs between Anthropic Messages and OpenAI Chat Completions).
  // We read `provider` from the per-session chatStore so the helper
  // can pick the right formula; falls back to null (= hide) for
  // providers that don't surface cache accounting (ollama, etc.).
  const sessionProvider = useChatStore((s) => s.agentStates[agentId]?.sessionStates[sessionId]?.provider ?? null);
  // Debug-mode gate for the per-turn cache-hit comparison row below.
  // Mirrors AppLayout's `isDebugMode` (`debug_state === "enabled"` and
  // `running`), so the extra diagnostic row appears exactly when the
  // Debug Panel is live — and disappears when DevMode is torn down.
  const agentDebugEnabled = useAgentStore(
    (s) => s.agents[agentId]?.meta?.debug_state === "enabled" && !!s.agents[agentId]?.meta?.alive,
  );
  const sendCompressAction = useChatStore((s) => s.sendCompressAction);
  const cancelCompressAction = useChatStore((s) => s.cancelCompressAction);
  // ADR-067: section byte sizes are emitted by the runtime's always-on
  // `process_llm_response_usage` path and live on `contextUsage.sections`
  // alongside the billed totals — no longer dependent on `useDebugStore`
  // (which is gated by DevMode and only fires on the debug snapshot path).
  // The popover is global UI; reading from chatStore ensures the
  // breakdown is populated even when the Debug Panel is closed.
  const sectionList = contextUsage?.sections ?? [];

  // ADR-066 §6: cache hit rate, provider-aware.  `null` means "no
  // signal" — either the provider doesn't report cache tokens, no LLM
  // call has happened yet, or the denominator would be zero.  We use
  // the session-lifetime (`cumulative`) window here to match the
  // right-hand agent-status panel's session-status block and the
  // bottom status bar — keeping one consistent number for the same
  // session rather than mixing per-turn (volatile) and cumulative
  // (stable) views across surfaces.
  const cacheStats = computeCacheHitStats(
    sessionProvider,
    contextUsage,
    "cumulative",
  );
  const cacheHitRateLabel = formatCacheHitRate(cacheStats.ratio);
  // ADR-066 §6: the real-time (per-turn) view of the last LLM call's
  // cache accounting. Debug-mode-only comparison row — the cumulative
  // row above is the stable session-lifetime signal, the per-turn row
  // exposes the volatile last-call signal for side-by-side testing.
  const perTurnCacheStats = computeCacheHitStats(
    sessionProvider,
    contextUsage,
    "per-turn",
  );
  const perTurnCacheHitRateLabel = formatCacheHitRate(perTurnCacheStats.ratio);

// Open popover on hover (not click), with a small delay before closing
  const handleMouseEnter = useCallback(() => {
    if (closeTimerRef.current) {
      clearTimeout(closeTimerRef.current);
      closeTimerRef.current = null;
    }
    setOpen(true);
  }, []);

  const handleMouseLeave = useCallback(() => {
    closeTimerRef.current = setTimeout(() => setOpen(false), 150);
  }, []);

  // Keep popover open while hovering over it
  const handlePopoverEnter = useCallback(() => {
    if (closeTimerRef.current) {
      clearTimeout(closeTimerRef.current);
      closeTimerRef.current = null;
    }
  }, []);

  useEffect(() => {
    return () => {
      if (closeTimerRef.current) clearTimeout(closeTimerRef.current);
    };
  }, []);

  const usagePercent = contextUsage?.usage_percent ?? 0;
  const usageBreakdown = computeContextUsageBreakdown(
    sectionList,
    usagePercent,
  );
  // ADR-049: derive from `getProcessingPhase()` instead of comparing status
  // string literals. The compiler checks exhaustiveness — adding a new
  // non-idle phase will not silently bypass this check.
  const isIdle = getProcessingPhase(sessionStatus) === "idle";
  // ADR-083: the compress button is dual-state, mirroring the ChatPanel
  // send→stop button. `canStart` gates idle→compress; `canCancel` is true
  // only while a compaction is in flight (the session is otherwise busy).
  const canStart = isIdle && !isCompacting && contextUsage != null;
  const canCancel = isCompacting;
  // ADR-076: the popover itself is a *read* surface (usage breakdown) and
  // stays openable on a read-only session, but both dual-state actions are
  // session writes — compressing rewrites history, and cancelling aborts the
  // owner's in-flight compaction. `canAct` is the single gate; the handlers
  // below re-check `canStart` / `canCancel` defensively.
  const readOnly = useSessionReadOnly(agentId, sessionId);
  const canAct = (canStart || canCancel) && !readOnly;

const handleCompressSummary = () => {
    if (!canStart || readOnly) return;
    // ADR-076 §决策 4: over HTTP now; COMPRESS_SUMMARY is the
    // `CompressType::SUMMARY` wire value.
    sendCompressAction(agentId, sessionId, COMPRESS_SUMMARY);
    setOpen(false);
  };

  const handleCancelCompact = () => {
    if (!canCancel || readOnly) return;
    // 3 = CompressType::CANCEL (ADR-083). The Runtime aborts the
    // distillation call and leaves history untouched.
    cancelCompressAction(agentId, sessionId);
    setOpen(false);
  };

  // ── ADR-074: per-session context window editor ───────────────────
  const sessionContextWindow = useChatStore(
    (s) => s.agentStates[agentId]?.sessionStates[sessionId]?.sessionContextWindow ?? null,
  );
  const activeModel = useChatStore(
    (s) => s.agentStates[agentId]?.sessionStates[sessionId]?.model ?? null,
  );
  const setSessionContextWindow = useChatStore((s) => s.setSessionContextWindow);

  const [editingWindow, setEditingWindow] = useState(false);
  const [windowDraft, setWindowDraft] = useState<string>("");
  const [windowError, setWindowError] = useState<string | null>(null);
  // Model window (for the "实际生效 = min(设定值, 模型窗口)" hint). Fetched
  // lazily once from the provider models API; `null` = unknown → hint off.
  const [modelWindow, setModelWindow] = useState<number | null>(null);
  const modelWindowFetched = useRef(false);

  const openWindowEditor = useCallback(() => {
    // Existing override → prefill in K units; no override → leave empty
    // with the placeholder showing the current effective total (ADR-074
    // §5.3 — the editor never introduces a second data source).
    setWindowDraft(sessionContextWindow != null ? String(sessionContextWindow / 1000) : "");
    setWindowError(null);
    setEditingWindow(true);
    if (!modelWindowFetched.current && sessionProvider && activeModel) {
      modelWindowFetched.current = true;
      fetchProviderModels(sessionProvider)
        .then((resp) => {
          const m = resp.models.find((mm) => mm.id === activeModel);
          if (m?.context_window) setModelWindow(m.context_window);
        })
        .catch(() => {
          // Unknown model window → hint stays off; the runtime still
          // caps at min(setting, model window).
        });
    }
  }, [sessionContextWindow, sessionProvider, activeModel]);

  const saveWindow = useCallback(() => {
    const trimmed = windowDraft.trim();
    if (trimmed === "") {
      // Empty input on save = don't send the field = don't write an
      // override (inheriting value must never be pinned as an override).
      setEditingWindow(false);
      return;
    }
    const abs = Math.round(parseFloat(trimmed) * 1000);
    if (!Number.isFinite(abs) || abs < WINDOW_FLOOR || abs > WINDOW_CEILING) {
      setWindowError(t("contextUsage.windowRangeError", {
        floor: WINDOW_FLOOR / 1000,
        ceiling: WINDOW_CEILING / 1000,
      }));
      return;
    }
    setWindowError(null);
    setSessionContextWindow(abs, agentId);
    setEditingWindow(false);
  }, [windowDraft, agentId, setSessionContextWindow, t]);

  // "实际生效 = min(设定值, 模型窗口)" hint, live while typing: shown
  // when the draft would be capped by the model window (the runtime
  // min's against the model window and never clamps the setting itself).
  const draftAbs = Number.isFinite(parseFloat(windowDraft))
    ? Math.round(parseFloat(windowDraft) * 1000)
    : null;
  const draftExceedsModel =
    editingWindow && draftAbs != null && modelWindow != null && draftAbs > modelWindow;

  // Same precision contract as `formatTokenCount` in `RightPanel.tsx`:
  // 2 decimals for M (= 10K granularity), 1 decimal for K.  Kept in
  // sync because the two values rendered side by side (e.g.
  // `2.71M / 2.78M` for cache vs. input) must speak the same
  // precision — otherwise a "2.7M vs 2.8M" pair would read as a
  // tie when the actual gap is 70K.
  const formatTokens = (n: number | undefined): string => {
    if (n == null) return "\u2014";
    if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
    if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
    return String(n);
  };

  return (
    <div
      className="relative"
      onMouseEnter={handleMouseEnter}
      onMouseLeave={handleMouseLeave}
    >
      {/* Icon button — shared right-cluster token so the hover rectangle
          matches the visibility / attachment / send buttons exactly. */}
      <button
        className={toolbarIconButton}
        aria-label={t("contextUsage.ariaLabel")}
      >
        {isCompacting ? (
          <span className="h-4 w-4 flex items-center justify-center">
            <span className="h-3 w-3 rounded-full border-2 border-[var(--color-accent)] border-t-transparent animate-spin" />
          </span>
        ) : (
          <CircularProgressIcon usagePercent={usagePercent} />
        )}
      </button>

      {/* Detailed usage popover. Sized and styled to match the sibling
          ModelMenu / SkillsPanel / SessionPanel dropdowns above the input
          toolbar (w-80, rounded-md, bg-modal-surface, zinc borders).
          The five-category list and stacked progress bar come from the
          latest debug context snapshot (see ADR-066). */}
      {open && (
        <div
          ref={popoverRef}
          role="dialog"
          aria-label={t("contextUsage.title")}
          onMouseEnter={handlePopoverEnter}
          onMouseLeave={handleMouseLeave}
          // ponytail: cap is viewport-bound so the editor expands naturally
          // without a scrollbar; the previous 460px ceiling was too low once
          // the inline window editor + presets + breakdown all rendered.
          className="absolute bottom-full right-0 z-50 mb-2 w-72 max-h-[calc(100vh-32px)] select-none overflow-y-auto overscroll-contain rounded-md border border-border-outer bg-modal-surface text-text-secondary shadow-lg "
        >
          <div className="flex items-center justify-between px-3 pt-2.5">
            <h2 className="text-sm font-normal text-text-secondary ">
              {t("contextUsage.title")}
            </h2>
            <button
              type="button"
              onClick={() => setOpen(false)}
              aria-label={t("contextUsage.close")}
              title={t("contextUsage.close")}
              className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-700  dark:hover:bg-zinc-700/50 dark:hover:text-zinc-100"
            >
              <X size={14} strokeWidth={2.25} />
            </button>
          </div>

          <div className="px-3 pt-2">
            <div className="flex items-center gap-2 whitespace-nowrap">
              <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-medium leading-none tracking-[-0.01em] text-text-secondary tabular-nums ">
                {formatDetailedPercent(usagePercent)}%
              </span>
              <span className="text-xs text-text-tertiary ">
                {t("contextUsage.used")} {" "}
                <span className="font-mono text-text-secondary ">
                  {formatTokens(contextUsage?.total_tokens ?? 0)}
                </span>
                <span className="text-text-tertiary "> / </span>
                <span className="font-mono text-text-secondary ">
                  {formatTokens(contextUsage?.context_window ?? 0)}
                </span>
              </span>
              {/* ADR-074: per-session window editor entry. */}
              <button
                type="button"
                onClick={openWindowEditor}
                disabled={readOnly}
                aria-label={t("contextUsage.editWindow")}
                title={readOnly ? t("chatPanel.readOnlySession") : t("contextUsage.editWindow")}
                className="ml-1 rounded p-0.5 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-700 disabled:cursor-not-allowed disabled:opacity-50 disabled:hover:bg-transparent  dark:hover:bg-zinc-700/50 dark:hover:text-zinc-100"
              >
                <Pencil size={11} strokeWidth={2.25} />
              </button>
            </div>

            {/* ADR-074: per-session window editor (inline, same popover). */}
            {editingWindow && (
              <div className="mt-2.5 rounded-md border border-border-outer p-2">
                <div className="flex items-center gap-1.5">
                  <input
                    type="number"
                    min={WINDOW_FLOOR / 1000}
                    step="any"
                    value={windowDraft}
                    onChange={(e) => {
                      setWindowDraft(e.target.value);
                      setWindowError(null);
                    }}
                    onKeyDown={(e) => {
                      if (e.key === "Enter") saveWindow();
                      if (e.key === "Escape") setEditingWindow(false);
                    }}
                    placeholder={formatTokens(contextUsage?.context_window ?? 0)}
                    aria-label={t("contextUsage.editWindow")}
                    className="w-full min-w-0 rounded border border-input-border bg-input-bg px-1.5 py-1 text-xs tabular-nums text-text-secondary outline-none focus:border-indigo-400 "
                  />
                  <span className="shrink-0 text-10 text-text-tertiary">{t("contextUsage.windowUnitK")}</span>
                </div>
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {WINDOW_PRESETS.map((preset) => (
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
                {draftExceedsModel && (
                  <p className="mt-1.5 text-10 leading-snug text-amber-600 dark:text-amber-400">
                    {t("contextUsage.windowEffectiveHint", { setting: formatTokens(draftAbs ?? 0), model: formatTokens(modelWindow ?? 0) })}
                  </p>
                )}
                {windowError != null && (
                  <p className="mt-1.5 text-10 leading-snug text-red-600 dark:text-red-400">{windowError}</p>
                )}
                <div className="mt-2 flex items-center gap-1.5">
                  <button
                    type="button"
                    onClick={saveWindow}
                    disabled={readOnly}
                    className="rounded bg-accent-strong px-2 py-1 text-11 font-medium text-white transition-[filter] hover:brightness-90 disabled:cursor-not-allowed disabled:opacity-50"
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

            <div
              className="mt-3 flex h-2 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700"
              role="progressbar"
              aria-label={t("contextUsage.usageBarLabel")}
              aria-valuemin={0}
              aria-valuemax={100}
              aria-valuenow={Math.floor(usagePercent)}
            >
              {usageBreakdown.map((category) => {
                if (category.percentage <= 0) return null;
                const meta = CATEGORY_META[category.key];
                return (
                  <span
                    key={category.key}
                    className="h-full transition-[width] duration-300"
                    style={{ width: `${category.percentage}%`, backgroundColor: meta.color }}
                  />
                );
              })}
            </div>
          </div>

          <div className="px-3 py-1">
            {usageBreakdown.map((category) => {
              const meta = CATEGORY_META[category.key];
              return (
                <div
                  key={category.key}
                  className="flex min-h-7 items-center gap-2 px-1 text-xs"
                  title={t(meta.labelKey)}
                >
                  <span
                    className="h-2 w-2 shrink-0 rounded-full"
                    style={{ backgroundColor: meta.color }}
                  />
                  <span className="font-normal text-text-secondary ">
                    {t(meta.labelKey)}
                  </span>
                  <span className="ml-auto font-mono tabular-nums text-text-secondary ">
                    {category.percentage.toFixed(1)}%
                  </span>
                </div>
              );
            })}
          </div>

          {hasCacheData(contextUsage, "cumulative") ? (
            <>
              <div className="border-t border-border-divider" />
              <div className="px-3 pt-2 pb-3">
                <div className="text-sm font-normal text-text-secondary ">
                  {t("contextUsage.cacheHitLabel")}
                </div>
                <div className="mt-1 flex items-baseline gap-2 whitespace-nowrap">
                  <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-medium leading-none tracking-[-0.01em] tabular-nums text-text-secondary ">
                    {cacheHitRateLabel ?? "\u2014"}
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
          ) : null}

          {agentDebugEnabled && hasCacheData(contextUsage, "per-turn") ? (
            <>
              <div className="border-t border-border-divider" />
              <div className="px-3 pt-2 pb-3">
                <div className="text-sm font-normal text-text-secondary ">
                  {t("contextUsage.cacheHitPerTurnLabel")}
                </div>
                <div className="mt-1 flex items-baseline gap-2 whitespace-nowrap">
                  <span className="text-[clamp(1.125rem,4.5vw,1.375rem)] font-semibold leading-none tracking-[-0.01em] tabular-nums text-text-secondary ">
                    {perTurnCacheHitRateLabel ?? "\u2014"}
                  </span>
                  <span className="text-xs text-text-tertiary ">
                    {t("contextUsage.cached")}{" "}
                    <span className="font-mono text-text-secondary ">
                      {formatTokens(perTurnCacheStats.numerator)}
                    </span>
                    <span className="text-text-tertiary "> / </span>
                    <span className="font-mono text-text-secondary ">
                      {formatTokens(perTurnCacheStats.denominator)}
                    </span>
                  </span>
                </div>
              </div>
            </>
          ) : null}

          <div className="border-t border-border-divider" />
          {/* ADR-083: dual-state button — idle → "compress summary";
              compacting → a clickable "cancel compaction" (accent colour +
              Square icon), same shape as the ChatPanel send→stop button. */}
          <button
            onClick={isCompacting ? handleCancelCompact : handleCompressSummary}
            disabled={!canAct}
            aria-label={isCompacting
              ? t("contextUsage.cancelCompact")
              : t("contextUsage.compressSummary")}
            className={cn(
              "mx-3 mb-2.5 mt-2 flex w-[calc(100%-1.5rem)] items-center justify-center gap-1.5 rounded-md px-3 py-1.5 text-xs font-medium transition-colors",
              isCompacting
                ? "bg-[var(--color-accent)]/10 text-[var(--color-accent)] hover:bg-[var(--color-accent)]/20"
                : "bg-zinc-100 text-text-secondary hover:bg-zinc-200 hover:text-zinc-900 dark:bg-white/10 dark:hover:bg-white/15 dark:hover:text-zinc-100",
              "disabled:cursor-not-allowed disabled:opacity-40",
            )}
          >
            {isCompacting ? (
              <>
                <Square size={14} fill="currentColor" />
                {t("contextUsage.cancelCompact")}
              </>
            ) : (
              <>
                <Minimize2 size={14} />
                {t("contextUsage.compressSummary")}
              </>
            )}
          </button>
        </div>
      )}
    </div>
  );
}
