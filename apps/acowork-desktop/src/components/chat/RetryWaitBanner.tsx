import { useEffect, useState, useRef, useCallback } from "react";
import { Clock, SkipForward, RefreshCw } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { bannerSlot } from "../../lib/ui-styles";

/**
 * Threshold (ms) above which the retry wait is treated as a "timeout retry"
 * (response timeout / stream timeout) rather than a 429 rate-limit retry.
 * 5 minutes = 300_000 ms.
 */
const TIMEOUT_RETRY_THRESHOLD_MS = 5 * 60 * 1000;

/**
 * Countdown banner shown when the LLM provider returns 429 (rate-limited)
 * or the LLM stream times out, and the retry wait exceeds 10 seconds.
 *
 * Two modes differ only in copy/icon (both share the same zinc banner chrome
 * with an accent-colored skip button):
 * - **429 mode** (`waitMs < 5 min`): "Rate limited" text, "Skip Wait" button.
 * - **Timeout mode** (`waitMs >= 5 min`): "Response timeout" text,
 *   "Retry Now" button.
 *
 * In both modes, a real-time countdown timer is displayed and the user can
 * click the button to skip the wait. When the timer expires, the backend
 * automatically retries the LLM request.
 *
 * ADR-014: the banner is derived directly from `sessionStatus` — the backend
 * owns the pause state; the frontend keeps no mirrored `retryWaitInfo` cache.
 * The countdown epoch (`startedAt`) is tracked locally: it is reset whenever
 * the retry info payload changes (new attempt / different wait), so the timer
 * stays aligned with the backend's wait window.
 *
 * IMPORTANT: this component owns its own `bannerSlot` wrapper and returns
 * `null` when not visible. Callers MUST NOT wrap `<RetryWaitBanner />` in
 * an outer wrapper, or an empty wrapper with `mt-1.5` will sit in the DOM
 * even when the banner is hidden, pushing sibling content below the chat
 * scroll viewport and causing a phantom scrollbar on empty sessions.
 */
export function RetryWaitBanner() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const currentSessionId = useChatStore((s) =>
    selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null,
  );
  const sessionStatus = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return null;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.sessionStatus ?? null;
  });

  // Derive retry info directly from the backend status — no mirrored cache.
  const retryInfo = sessionStatus?.status === "paused" ? sessionStatus.detail?.retry_info ?? null : null;

  // Local countdown epoch — reset whenever the retry payload changes so the
  // timer reflects the backend's wait window (a new 429 restarts the count).
  const [startedAt, setStartedAt] = useState<number>(0);
  const lastRetryKeyRef = useRef<string | null>(null);
  useEffect(() => {
    const key = retryInfo ? `${retryInfo.wait_ms}:${retryInfo.attempt}` : null;
    if (key !== lastRetryKeyRef.current) {
      lastRetryKeyRef.current = key;
      setStartedAt(retryInfo ? Date.now() : 0);
    }
  }, [retryInfo]);

  // Determine if this is a timeout retry (long wait) or 429 retry (short wait)
  const isTimeoutMode = retryInfo !== null && retryInfo.wait_ms >= TIMEOUT_RETRY_THRESHOLD_MS;

  // Local countdown state — derived from startedAt + waitMs
  const [remainingMs, setRemainingMs] = useState<number>(0);
  const rafRef = useRef<number | null>(null);

  // Recalculate remaining time every animation frame for smooth countdown
  const tick = useCallback(() => {
    if (!retryInfo) {
      setRemainingMs(0);
      return;
    }
    const elapsed = Date.now() - startedAt;
    const remaining = Math.max(0, retryInfo.wait_ms - elapsed);
    setRemainingMs(remaining);
    if (remaining > 0) {
      rafRef.current = requestAnimationFrame(tick);
    }
  }, [retryInfo, startedAt]);

  useEffect(() => {
    if (retryInfo) {
      rafRef.current = requestAnimationFrame(tick);
    } else {
      setRemainingMs(0);
    }
    return () => {
      if (rafRef.current !== null) {
        cancelAnimationFrame(rafRef.current);
        rafRef.current = null;
      }
    };
  }, [retryInfo, tick]);

  const handleSkip = () => {
    if (selectedAgentId) {
      void useChatStore.getState().continueExecution(selectedAgentId);
    }
  };

  if (!retryInfo || !selectedAgentId || !currentSessionId) return null;

  const remainingSec = Math.ceil(remainingMs / 1000);
  const totalSec = Math.ceil(retryInfo.wait_ms / 1000);

  const label = isTimeoutMode
    ? "Response timeout"
    : "Rate limited";

  const buttonLabel = isTimeoutMode
    ? "Retry Now"
    : "Skip Wait";

  const ButtonIcon = isTimeoutMode ? RefreshCw : SkipForward;

  return (
    <div className={bannerSlot}>
      <div
        role="status"
        aria-live="polite"
        className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-border-outer bg-zinc-50 px-4 py-2 text-text-secondary select-none  dark:bg-zinc-800/40 "
        style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
      >
        <span className="flex shrink-0 items-center gap-1.5">
          <Clock className="h-3.5 w-3.5 text-text-tertiary " />
          <span className="text-xs font-medium">
            {label} — retrying in{" "}
            <span className="tabular-nums font-mono font-bold">
              {remainingSec}s
            </span>
            {" "}({retryInfo.attempt}/{retryInfo.max_attempts})
          </span>
        </span>

        <span className="hidden sm:inline text-11 text-text-tertiary/70 ">
          {retryInfo.provider}
        </span>

        <div className="ml-auto flex items-center gap-1.5">
          {/* Countdown progress bar */}
          <div className="hidden sm:block h-1.5 w-16 rounded-full bg-zinc-200 dark:bg-zinc-700">
            <div
              className="h-full rounded-full bg-zinc-400 transition-[width] duration-1000 ease-linear dark:bg-zinc-500"
              style={{
                width: `${Math.max(0, Math.min(100, ((totalSec - remainingSec) / totalSec) * 100))}%`,
              }}
            />
          </div>

          <button
            type="button"
            onClick={handleSkip}
            className="flex items-center gap-1 rounded bg-accent-strong px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
          >
            <ButtonIcon className="h-3 w-3" />
            {buttonLabel}
          </button>
        </div>
      </div>
    </div>
  );
}
