import React, { useState, useRef, useEffect } from "react";
import { ChevronRight, ChevronDown, Atom, Sparkles } from "lucide-react";
import { useTranslation } from "../../i18n/useTranslation";

/**
 * ADR-035 / D4 streaming source block.
 *
 * Memory-efficient streaming preview used by both the in-progress
 * `thought` (inside ExploreBlock) and the in-progress `assistant`
 * reply (trailing virtual item replacing the "Replying..." indicator).
 *
 * Same DOM node is reused for the entire lifetime of the block; each
 * tick overwrites the <pre>'s textContent via direct DOM mutation.
 * No ReactMarkdown AST allocation, no React element tree churn —
 * flat memory during streaming.
 *
 * Visual chrome (border-l-2 gutter, header row with icon + label +
 * duration + chevron, content <pre>) is shared between both variants;
 * only the icon, labels, and expand/collapse policy differ.
 */

/** Max visible lines in the content area (overflow scrolls). */
const DEFAULT_MAX_VISIBLE_LINES = 5;
/** text-sm line-height */
const LINE_HEIGHT_REM = 1.5;
/** Font size: 90% of app font size, matches ExploreBlock items */
const HEADER_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
const DURATION_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

export interface StreamingSourceBlockProps {
  /** Streaming content (already truncated to displayable size upstream). */
  content: string;
  /** Whether the block is currently streaming. */
  isStreaming: boolean;
  /** Start time for duration timer. */
  startTime?: number;
  /** End time when streaming completes (frozen state). */
  endTime?: number;
  /** Visual variant — determines icon, labels, and collapse policy. */
  variant: "thought" | "assistant";
  /** Default expanded state on mount. Defaults to isStreaming. */
  defaultExpanded?: boolean;
  /** Max visible lines in the content area. Default 5. */
  maxVisibleLines?: number;
  /** Whether to show the "showing latest N lines" notice. Default true. */
  showTruncationNotice?: boolean;
}

/**
 * Collapsible streaming source block.
 *
 * Variant behavior:
 * - `thought`: atom icon, "Thinking"/"Thought" labels, auto-collapses
 *   on completion. Used both as live preview (inside ExploreBlock)
 *   and as frozen display in the message list.
 * - `assistant`: sparkles icon, "Replying" label only (slot disappears
 *   on record_complete). Used as the trailing virtual item replacing
 *   the old "Replying..." indicator.
 */
export const StreamingSourceBlock = React.memo(function StreamingSourceBlock({
  content,
  isStreaming,
  startTime,
  endTime,
  variant,
  defaultExpanded,
  maxVisibleLines = DEFAULT_MAX_VISIBLE_LINES,
  showTruncationNotice = true,
}: StreamingSourceBlockProps) {
  const { t } = useTranslation();
  const hasEndTime = endTime != null;
  const isThinking = variant === "thought" && isStreaming && !hasEndTime;
  const isStreamingAssistant = variant === "assistant" && isStreaming && !hasEndTime;
  const isActive = isThinking || isStreamingAssistant;

  // Thought blocks default to collapsed: fold state is entirely the user's
  // call (click the caret to reveal).  Only the trailing assistant slot
  // auto-expands during streaming — it's the placeholder for the live
  // reply, which the user explicitly wants to see arriving.
  const [expanded, setExpanded] = useState(
    variant === "assistant" ? isActive : (defaultExpanded ?? false)
  );
  const [, setTick] = useState(0);
  const preRef = useRef<HTMLPreElement>(null);
  const manuallyCollapsed = useRef(false);

  // Direct textContent mutation: reuse the same <pre> DOM node, only
  // overwrite its text on each tick.  Zero AST allocation, zero DOM tree
  // churn — the <pre> is created once and stays alive for the entire
  // lifetime of the block.
  useEffect(() => {
    if (preRef.current && preRef.current.textContent !== content) {
      preRef.current.textContent = content || "...";
    }
  }, [content]);

  // Live timer tick: ensure duration updates every second even when no
  // new stream_delta arrives (e.g. slow thinking phase between chunks).
  useEffect(() => {
    if (!isActive) return;
    const interval = setInterval(() => setTick((t) => t + 1), 1000);
    return () => clearInterval(interval);
  }, [isActive]);

  // Auto-expand applies only to the assistant slot.  Thought blocks never
  // auto-expand or auto-collapse — once the user opens one, it stays open
  // until they collapse it; once closed, it stays closed.
  useEffect(() => {
    if (variant === "assistant" && isActive && !manuallyCollapsed.current) {
      setExpanded(true);
    }
  }, [variant, isActive]);

  // Duration: use fixed endTime if available, otherwise live timer
  const duration = startTime
    ? Math.round(((endTime ?? Date.now()) - startTime) / 1000)
    : null;

  // Variant-specific chrome: icon, label, whether to render header button
  const Icon = variant === "thought" ? Atom : Sparkles;
  const label =
    variant === "thought"
      ? (!isStreaming || hasEndTime)
        ? t("thinkBlock.thought")
        : t("thinkBlock.thinking")
      : t("chatPanel.replying");

  return (
    // `max-w-[var(--content-max-width)]` mirrors the width contract shared by
    // every other chat bubble in the panel (MessageBubble / ExploreBlock /
    // CompactionCard / AskQuestionCard).  It is driven by the global
    // "display content width" setting written by settingsStore.  Without it,
    // the inner `w-full` content area would stretch to fill the chat panel
    // and visually drift past adjacent MessageBubble columns — most visible
    // in the trailing "Replying" slot in VirtualMessageList, where the
    // StreamingSourceBlock is mounted as a direct child of the chat scroll
    // container rather than inside an already-constrained bubble wrapper.
    <div className="my-1 max-w-[var(--content-max-width)]">
      <button
        onClick={() => {
          const next = !expanded;
          setExpanded(next);
          if (!next && isActive) {
            manuallyCollapsed.current = true;
          } else if (next) {
            manuallyCollapsed.current = false;
          }
        }}
        className="flex items-center gap-2 text-text-tertiary hover:text-zinc-700  dark:hover:text-zinc-300 transition-colors"
        style={{ fontSize: HEADER_FONT_SIZE }}
      >
        <Icon className="h-3 w-3 shrink-0" />
        <span>{label}</span>
        {duration !== null && (
          <span style={{ fontSize: DURATION_FONT_SIZE }}>({duration}s)</span>
        )}
        {expanded ? (
          <ChevronDown className="h-3 w-3" />
        ) : (
          <ChevronRight className="h-3 w-3" />
        )}
      </button>

      {expanded && (
        <div
          className="w-full ml-5 mt-1 pl-3 py-2 bg-zinc-50 dark:bg-zinc-800/50 text-text-tertiary  border-l-2 border-zinc-300 dark:border-zinc-600 overflow-hidden"
          style={{ maxHeight: `${maxVisibleLines * LINE_HEIGHT_REM}rem` }}
        >
          {showTruncationNotice && (
            <div className="text-xs text-text-tertiary  italic mb-1 select-none">
              … ({t("thinkBlock.showingLatest")})
            </div>
          )}
          {/* Source-code rendering: <pre> with direct textContent mutation.
              Same DOM node reused for entire block lifetime.  No AST, no
              element tree churn — flat memory during streaming. */}
          <pre
            ref={preRef}
            className="text-sm whitespace-pre-wrap break-words m-0 font-mono text-text-tertiary "
            style={{ fontSize: "var(--ui-font-size, 0.875rem)", lineHeight: LINE_HEIGHT_REM }}
          >
            {content || "..."}
          </pre>
        </div>
      )}
    </div>
  );
}, (prev, next) => {
  // Only re-render when observable output changes, not on every poll tick.
  // startTime is excluded — it's set once at creation and never changes.
  return prev.content === next.content
    && prev.isStreaming === next.isStreaming
    && prev.endTime === next.endTime
    && prev.variant === next.variant
    && prev.maxVisibleLines === next.maxVisibleLines
    && prev.showTruncationNotice === next.showTruncationNotice;
});