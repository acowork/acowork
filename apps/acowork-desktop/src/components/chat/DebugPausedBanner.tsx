import { useEffect } from "react";
import { Bug, Pause, Play, StepForward } from "lucide-react";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useDebugStore } from "../../stores/debugStore";
import { useTranslation } from "../../i18n/useTranslation";
import { bannerSlot } from "../../lib/ui-styles";

/**
 * Banner shown inside the chat panel when the selected agent is in debug mode
 * AND the debugger is currently in a paused/stepping state.
 *
 * Mirrors the visual style of the iteration-limit-paused banner but with
 * debug-specific colors and resume/step actions. The banner disappears
 * automatically when the debugger transitions back to "Running".
 *
 * IMPORTANT: this component owns its own `bannerSlot` wrapper and returns
 * `null` when not visible. Callers MUST NOT wrap `<DebugPausedBanner />` in
 * an outer wrapper, or an empty wrapper with `mt-1.5` will sit in the DOM
 * even when the banner is hidden, pushing sibling content below the chat
 * scroll viewport and causing a phantom scrollbar on empty sessions.
 */
export function DebugPausedBanner() {
  const { t } = useTranslation();
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  // ADR-048 follow-up: key off `debug_state` (current capability), not
  // `dev_mode` (startup intent) — DevMode can be flipped on at runtime.
  const devMode = useAgentStore((s) => {
    if (!s.selectedAgentId) return false;
    return s.agents[s.selectedAgentId]?.meta?.debug_state === "enabled";
  });
  const currentSessionId = useChatStore((s) =>
    selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null,
  );
  const sessionDebugState = useDebugStore((s) =>
    currentSessionId ? s.sessionStates[currentSessionId] ?? null : null,
  );
  const debugState = sessionDebugState?.debugState ?? null;

  const isPaused = sessionDebugState?.paused === true;
  const visible = devMode && isPaused && !!currentSessionId;

  // Register F5 (resume) and F10 (step) shortcuts when banner is visible.
  // We use the capture phase so we run before main.tsx's bubble-phase
  // handler. The Monaco editor guard prevents stealing shortcuts while
  // the user is debugging source code in the file editor.
  useEffect(() => {
    if (!visible || !currentSessionId) return;

    const handler = (e: KeyboardEvent) => {
      if (e.ctrlKey || e.altKey || e.metaKey || e.shiftKey) return;
      const target = e.target;
      if (target instanceof Element && target.closest(".monaco-editor")) return;

      if (e.key === "F5") {
        e.preventDefault();
        e.stopPropagation();
        void useDebugStore.getState().resume(currentSessionId);
        return;
      }
      if (e.key === "F10") {
        e.preventDefault();
        e.stopPropagation();
        void useDebugStore.getState().step(currentSessionId, "iteration");
        return;
      }
    };

    document.addEventListener("keydown", handler, true);
    return () => document.removeEventListener("keydown", handler, true);
  }, [visible, currentSessionId]);

  if (!visible || !currentSessionId) return null;

  const handleResume = () => {
    void useDebugStore.getState().resume(currentSessionId);
  };
  const handleStep = () => {
    void useDebugStore.getState().step(currentSessionId, "iteration");
  };

  const stateLabel = debugState === "Paused" ? t("debugPausedBanner.statePaused") : t("debugPausedBanner.stateStepping");

  return (
    <div className={bannerSlot}>
      <div
        role="status"
        aria-live="polite"
        className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-border-outer bg-zinc-50 px-4 py-2 text-text-secondary select-none  dark:bg-zinc-800/40 "
        style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
      >
        <span className="flex shrink-0 items-center gap-1.5">
          <Bug className="h-3.5 w-3.5 text-text-tertiary " />
          <Pause className="h-3 w-3 text-text-tertiary " />
          <span className="text-xs font-medium">
            {stateLabel} in debug mode
          </span>
        </span>

        <div className="ml-auto flex items-center gap-1.5">
          <button
            type="button"
            onClick={handleResume}
            className="flex items-center gap-1 rounded bg-accent-strong px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
          >
            <Play className="h-3 w-3" fill="currentColor" />
            <span>{t("debugPausedBanner.resume")}</span>
            <KbdHint>F5</KbdHint>
          </button>
          <button
            type="button"
            onClick={handleStep}
            className="flex items-center gap-1 rounded bg-accent-strong px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
          >
            <StepForward className="h-3 w-3" />
            <span>{t("debugPausedBanner.step")}</span>
            <KbdHint>F10</KbdHint>
          </button>
        </div>
      </div>
    </div>
  );
}

function KbdHint({ children }: { children: React.ReactNode }) {
  return (
    <kbd className="ml-0.5 rounded bg-black/15 px-1 py-px font-mono text-9 font-semibold leading-none dark:bg-white/15">
      {children}
    </kbd>
  );
}
