import React, { useState, useRef, useEffect, useCallback } from "react";
import { ChevronRight, ChevronDown, Search, Wrench, Terminal, Check, X, Square } from "lucide-react";
import type { ChatMessage, ToolApprovalNeededEvent } from "../../lib/types";
import { ThinkBlock } from "./ThinkBlock";
import { useTranslation } from "../../i18n/useTranslation";
import { OverlayScrollbar } from "../common/OverlayScrollbar";

interface ExploreBlockProps {
  items: ChatMessage[];
  isStreaming: boolean;
  /** Map of tool_call_id → approval event for precise matching with tool call items. */
  pendingApproval?: Record<string, ToolApprovalNeededEvent> | null;
  currentSessionId?: string | null;
  onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void;
  /** ADR-045: Cancel a single in-flight tool execution. */
  onCancelTool?: (toolCallId: string) => void;
  /** ADR-045: Per-tool progress heartbeat state, keyed by tool_call_id. */
  toolProgress?: Record<string, { elapsedMs: number; timeoutMs: number }>;
  /** True when an assistant reply message follows this explore block in display order.
   *  This is the ONLY condition that triggers auto-collapse. */
  hasFollowUpReply?: boolean;
  /** Live thinking state from the store.  When `isThinking=true` AND the
   *  thought message has not yet been loaded into `items[]` (i.e. HTTP
   *  refresh hasn't completed), a lightweight live ThinkBlock is rendered
   *  at the top of the expanded view so the user sees real-time progress
   *  inside the explore block — not floating outside as a duplicate. */
  isThinking?: boolean;
  thinkingContent?: string;
  thinkingStartTime?: number | null;
  /**
   * ADR-050: true iff at least one item in this block originated from the
   * adapter's liveBuffer (streaming preview).  When `isLive=true`, the
   * live thought from `liveBuffer.thinkingStream` is already folded into
   * `items[]` by the adapter - so the `liveThoughtNotYetLoaded` ThinkBlock
   * must NOT render (it would duplicate the PairedExploreItem that already
   * renders the same thought).  When `isLive=false` (user scrolled away
   * from the tail, liveBuffer entries are excluded from blocks), the live
   * ThinkBlock is the only way to show the streaming preview.
   */
  isLive?: boolean;
}

const SHELL_TOOLS = ["bash", "powershell", "shell"];

/** Font size for ExploreBlock content: 90% of app font size */
const EXPLORE_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.9)";
/** Font size for detail panels (params/result): 80% of app font size */
const EXPLORE_DETAIL_FONT_SIZE = "calc(var(--ui-font-size, 0.875rem) * 0.8)";

function isShellTool(name: string): boolean {
  return SHELL_TOOLS.includes(name);
}
/** Format milliseconds as M:SS or H:MM:SS for the heartbeat timer. */
function formatDuration(ms: number): string {
  const totalSec = Math.floor(ms / 1000);
  const h = Math.floor(totalSec / 3600);
  const m = Math.floor((totalSec % 3600) / 60);
  const s = totalSec % 60;
  const mm = m.toString().padStart(h > 0 ? 2 : 1, "0");
  const ss = s.toString().padStart(2, "0");
  return h > 0 ? `${h}:${mm}:${ss}` : `${mm}:${ss}`;
}


/**
 * Build the one-line summary that appears right of a tool name in the
 * ExploreBlock chip, e.g. `file_read  src/foo.rs (L1-L50)`.
 *
 * Per-tool rules — derived from the Rust builtin schemas under
 * `core/acowork-runtime/src/tools/builtin/*.rs`:
 *  - shell              : command (verbatim, truncated to 60 chars)
 *  - file_read          : path (L{start_line}–L{end_line})
 *  - file_write         : path [mode=append]
 *  - file_edit          : path (N chars)
 *  - doc_reader         : path [P{a}-{b}]   (pages/sheets/slides)
 *  - http_request       : METHOD url
 *  - web_fetch          : url
 *  - content_search     : pattern [in path]  [(N matches|no matches)]
 *  - glob_search        : pattern [in path]  [(N files|no matches)]
 *  - memory_recall      : query (N hits)
 *  - memory_store       : <content, truncated> (category)
 *  - rag_query          : query (top_k)
 *  - intent_send        : target → action
 *  - ask_user_question  : <question, truncated>
 *  - todo_write         : N items (M done)
 *  - mcp_install        : name (transport)
 *  - mcp_uninstall      : name
 *  - codebase           : action [language] [file:Lline]
 *  - other (MCP/external): no special handling — falls through to first field
 */
function summarizeToolCall(
  toolName: string,
  params: Record<string, unknown>,
  result?: ChatMessage,
  isShell = false,
): string {
  const asString = (v: unknown): string => (typeof v === "string" ? v : "");
  const truncate = (s: string, max = 60): string =>
    s.length > max ? s.slice(0, max - 1) + "…" : s;

  // Helper: extract "Total: N" footer from a tool result.
  const extractTotal = (re: RegExp, emptyMatch: string): string | null => {
    if (!result) return null;
    const m = result.content.match(re);
    if (m) return m[1];
    if (/no matches found|no files matched/i.test(result.content)) return emptyMatch;
    return null;
  };

  if (isShell) {
    return truncate(asString(params.command));
  }

  switch (toolName) {
    case "file_read": {
      const path = asString(params.path);
      const sl = params.start_line as number | undefined;
      const el = params.end_line as number | undefined;
      return sl != null || el != null
        ? `${path} (L${sl ?? "?"}–L${el ?? "?"})`
        : path;
    }
    case "file_write": {
      const path = asString(params.path);
      const mode = asString(params.mode);
      return mode && mode !== "overwrite" ? `${path} [${mode}]` : path;
    }
    case "file_edit": {
      const path = asString(params.path);
      const newText = asString(params.new_text);
      const sz = newText.length;
      return sz > 0 ? `${path} (${sz} chars)` : path;
    }
    case "doc_reader": {
      const path = asString(params.path);
      const sp = params.start_page as number | undefined;
      const ep = params.end_page as number | undefined;
      return sp != null || ep != null
        ? `${path} [P${sp ?? "?"}–${ep ?? "?"}]`
        : path;
    }
    case "http_request": {
      const method = asString(params.method) || "GET";
      const url = asString(params.url);
      return url ? `${method} ${url}` : "";
    }
    case "web_fetch":
      return asString(params.url);
    case "content_search": {
      let s = asString(params.pattern);
      const path = asString(params.path);
      if (path) s += ` in ${path}`;
      const total = extractTotal(/Total:\s*(\d+)\b/i, "no matches");
      return total ? `${s} (${total})` : s;
    }
    case "glob_search": {
      let s = asString(params.pattern);
      const path = asString(params.path);
      if (path) s += ` in ${path}`;
      const total = extractTotal(/Total:\s*(\d+)\s*files/i, "no matches");
      return total ? `${s} (${total})` : s;
    }
    case "memory_recall": {
      const q = asString(params.query);
      const total = extractTotal(/Found\s+(\d+)|Total:\s*(\d+)/i, "0 hits");
      // Note: regex may put capture in group 1 OR 2; pick whichever matched.
      if (total) {
        const n = (result?.content.match(/Found\s+(\d+)|Total:\s*(\d+)/i) ?? [])[1]
          || (result?.content.match(/Found\s+(\d+)|Total:\s*(\d+)/i) ?? [])[2];
        return q ? `${q} (${n || total})` : `(recall) (${n || total})`;
      }
      return q || "(recall)";
    }
    case "memory_store": {
      const content = truncate(asString(params.content), 40);
      const category = asString(params.category);
      return category ? `${content} (${category})` : content;
    }
    case "rag_query": {
      const q = asString(params.query);
      const k = params.top_k as number | undefined;
      return k != null ? `${q} (top ${k})` : q;
    }
    case "intent_send": {
      const target = asString(params.target);
      const action = asString(params.action);
      return target && action ? `${target} → ${action}` : target || action;
    }
    case "ask_user_question": {
      const q = truncate(asString(params.question), 50);
      const opts = Array.isArray(params.options) ? params.options.length : 0;
      return opts > 0 ? `${q} (${opts} options)` : q;
    }
    case "todo_write": {
      const todos = params.todos;
      if (Array.isArray(todos)) {
        const total = todos.length;
        const completed = todos.filter(
          (t) => t && typeof t === "object" && (t as { status?: unknown }).status === "completed",
        ).length;
        return `${total} ${total === 1 ? "item" : "items"}${completed > 0 ? ` (${completed} done)` : ""}`;
      }
      return "";
    }
    case "mcp_install": {
      const name = asString(params.name);
      const transport = asString(params.transport) || "stdio";
      return `${name} (${transport})`;
    }
    case "mcp_uninstall":
      return asString(params.name);
    case "codebase": {
      const action = asString(params.action) || "?";
      const file = asString(params.file);
      const line = params.line as number | undefined;
      const char = params.character as number | undefined;
      if (file && line != null) {
        return `${action} ${file}:${line}${char != null ? `:${char}` : ""}`;
      }
      const q = asString(params.query);
      return q ? `${action} "${truncate(q, 30)}"` : action;
    }
    default: {
      // Fallback: pick the first non-empty string field by name preference.
      for (const key of ["path", "pattern", "query", "url", "name", "command", "target"]) {
        const v = asString(params[key]);
        if (v) return v;
      }
      // Last resort: first key + a safe, type-aware value preview.
      const entries = Object.entries(params);
      if (entries.length === 0) return "";
      const [key, value] = entries[0];
      let preview: string;
      if (typeof value === "string") preview = value;
      else if (typeof value === "number" || typeof value === "boolean") preview = String(value);
      else if (Array.isArray(value)) preview = `[${value.length}]`;
      else if (value && typeof value === "object") preview = "{…}";
      else preview = String(value);
      return `${key}: ${preview.slice(0, 60)}`;
    }
  }
}

/** Check if a specific approval event belongs to the current session.
 *  If session_id is absent (old Runtime), assume it matches (backward compat). */
function approvalMatchesSession(
  approval: ToolApprovalNeededEvent,
  currentSessionId?: string | null,
): boolean {
  if (approval.session_id === undefined || approval.session_id === null) return true;
  return approval.session_id === currentSessionId;
}

/**
 * ExploreBlock: aggregates consecutive think + tool_call + tool_result
 * messages into a single collapsible block with full rendering inside.
 *
 * - Default: expanded (for new active blocks).
 * - Collapsed: "Exploring... (N steps)" + chevron.
 * - Expanded: max-height 144px container with ThinkBlock and ToolCallItem.
 * - Streaming: auto-scrolls to bottom.
 * - Collapse (auto): ONLY when hasFollowUpReply=true — an assistant reply
 *   message appears after this explore block in display order.
 * - Collapse (manual): user can collapse at any time.
 */
export const ExploreBlock = React.memo(function ExploreBlock({ items, isStreaming, pendingApproval, currentSessionId, onApprove, hasFollowUpReply, onCancelTool, toolProgress, isThinking, thinkingContent, thinkingStartTime, isLive }: ExploreBlockProps) {
  const { t } = useTranslation();
  // Start collapsed only if this block already has a follow-up reply (historical/loaded).
  // For new active blocks, always start expanded — collapses ONLY when
  // an assistant reply appears after it.
  const [expanded, setExpanded] = useState(!hasFollowUpReply);
  const contentRef = useRef<HTMLDivElement>(null);
  const manuallyCollapsed = useRef(false);

  // Auto-scroll to bottom when expanded and new items arrive
  useEffect(() => {
    if (expanded && contentRef.current) {
      contentRef.current.scrollTop = contentRef.current.scrollHeight;
    }
  }, [expanded, items, thinkingContent]);

  const pairedItems = buildPairedItems(items);
  const stepCount = pairedItems.length;

  // Count tool_calls that have NOT yet been paired with a tool_result.
  // Derived once here so both `hasPendingTools` (drives isExploring /
  // auto-expand) and the collapsed-header "M running" badge can share
  // the same source of truth instead of recomputing over `pairedItems`
  // a second time.
  const pendingToolsCount = pairedItems.filter(
    (item) => item.kind === "tool" && !item.result
  ).length;

  // Still have tool_calls without results
  const hasPendingTools = pendingToolsCount > 0;

  const isExploring = isStreaming || hasPendingTools;

  // Live ThinkBlock: shown only when (a) the agent is actively thinking,
  // AND (b) the live thought is NOT already folded into this block's
  // items[] by the adapter.
  //
  // ADR-050: when atTail, the adapter folds `liveBuffer.thinkingStream`
  // into the explore_group's items[] and marks the block `isLive: true`.
  // In that case the PairedExploreItem already renders the thought, so
  // the live ThinkBlock must NOT render (it would be a duplicate).
  //
  // When NOT atTail (user scrolled away), the adapter excludes
  // liveBuffer entries from blocks, so `isLive=false` and the live
  // ThinkBlock is the only way to show the streaming preview.
  //
  // Pre-ADR-050, this checked `pairedItems[0].kind !== "thought"`, which
  // only looked at the FIRST item.  When tool_calls preceded the live
  // thought in the same explore_group, the check passed incorrectly and
  // the live ThinkBlock rendered as a duplicate of PairedExploreItem.
  const liveThoughtNotYetLoaded = !!isThinking && !isLive;

  // Auto-expand when exploring starts (respect user manual collapse),
  // but only if no follow-up reply has appeared (once collapsed by reply, stay collapsed)
  useEffect(() => {
    if (isExploring && !hasFollowUpReply && !manuallyCollapsed.current) {
      setExpanded(true);
    }
  }, [isExploring, hasFollowUpReply]);

  // Auto-collapse when agent response appears after this explore block.
  // This is the ONLY auto-collapse condition — tools finishing alone does NOT collapse.
  useEffect(() => {
    if (hasFollowUpReply) {
      setExpanded(false);
      manuallyCollapsed.current = false;
    }
  }, [hasFollowUpReply]);

  // Check if this block has any pending shell approval for current session
  const hasPendingApproval = pendingApproval && Object.values(pendingApproval).some(
    (ev) => {
      const sessionMatch = approvalMatchesSession(ev, currentSessionId);
      const toolMatch = items.some(
        (m) => m.type === "tool_call" && m.toolCallId === ev.tool_call_id && !items.some(
          (r) => r.type === "tool_result" && r.toolName === m.toolName
        )
      );
      return sessionMatch && toolMatch;
    }
  );

  // Auto-expand when pending approval — always, even if user collapsed
  useEffect(() => {
    if (hasPendingApproval) {
      setExpanded(true);
      manuallyCollapsed.current = false;
      // Auto-scroll to bottom so the approval button is visible
      setTimeout(() => {
        if (contentRef.current) {
          contentRef.current.scrollTop = contentRef.current.scrollHeight;
        }
      }, 0);
    }
  }, [hasPendingApproval]);

  return (
    <div className="max-w-[var(--content-max-width)]">
      {/* Header: clickable toggle */}
      <button
        onClick={() => {
          const next = !expanded;
          setExpanded(next);
          // Track manual collapse during exploring; reset on manual expand
          if (!next && isExploring) {
            manuallyCollapsed.current = true;
          } else if (next) {
            manuallyCollapsed.current = false;
          }
        }}
        className="flex w-fit items-center gap-2 rounded-md bg-zinc-100/60 px-2.5 py-1.5 text-text-tertiary transition-colors hover:bg-zinc-200 dark:bg-zinc-800/30  dark:hover:bg-zinc-700"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <Search className="h-3.5 w-3.5 shrink-0 text-text-tertiary " />
        <span className="font-medium text-text-tertiary ">
          {hasFollowUpReply ? t("exploreBlock.explored") : t("exploreBlock.exploring")}
        </span>
        <span className="text-text-tertiary ">
          ({t("exploreBlock.step", { count: stepCount })}
          {/* When collapsed and at least one tool is still awaiting its
              result, append a "· M running" suffix so the user can see
              progress without expanding the block. Suppressed when expanded
              (the per-row status icons already convey the same info) and
              when the block has a follow-up reply (no work in flight). */}
          {!expanded && !hasFollowUpReply && hasPendingTools && (
            <>
              {" · "}
              {t("exploreBlock.running", { count: pendingToolsCount })}
            </>
          )}
          )
        </span>
        {expanded ? (
          <ChevronDown className="ml-auto h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        ) : (
          <ChevronRight className="ml-auto h-3.5 w-3.5 shrink-0 text-text-tertiary" />
        )}
      </button>

      {/* Expanded content: full ThinkBlock + paired ToolCall rendering */}
      {expanded && (
        <OverlayScrollbar
          ref={contentRef}
          maxHeight={144}
          containerClassName="flex-none max-h-60 ml-2 mt-1 rounded-md border-l-2 border-zinc-300 bg-zinc-100/60 dark:border-zinc-600 dark:bg-zinc-800/30"
          className="pl-3 pr-2 py-2 overscroll-contain"
          onViewportWheel={(event) => {
            if (event.deltaY !== 0) event.stopPropagation();
          }}
        >
          <div className="flex flex-col gap-0.5">
            {pairedItems.map((paired, idx) => (
              <PairedExploreItem key={idx} item={paired} isStreaming={isStreaming} pendingApproval={pendingApproval} currentSessionId={currentSessionId} onApprove={onApprove} onCancelTool={onCancelTool} toolProgress={toolProgress} />
            ))}
            {/* Live thought anchored at the BOTTOM (after paired items).
                Rationale:  the live thought is the freshest item currently
                being produced, so it should be the last / bottom-most
                entry the user sees.  Once HTTP refresh lands the thought
                into items[], buildPairedItems will hoist it into its
                proper chronological position above this slot, and the
                live ThinkBlock disappears (liveThoughtNotYetLoaded=false).
                The container auto-scrolls to scrollHeight (see useEffect
                above) so the bottom-anchored live thought is always in
                view while it streams. */}
            {liveThoughtNotYetLoaded && (
              <ThinkBlock
                content={thinkingContent ?? ""}
                isStreaming={true}
                startTime={thinkingStartTime ?? undefined}
              />
            )}
          </div>
        </OverlayScrollbar>
      )}
    </div>
  );
}, (prev, next) => {
  // items reference changes only when new explore items arrive.
  // onApprove is excluded — it's an inline callback that changes every render.
  return prev.items === next.items
    && prev.isStreaming === next.isStreaming
    && prev.pendingApproval === next.pendingApproval
    && prev.hasFollowUpReply === next.hasFollowUpReply
    && prev.toolProgress === next.toolProgress
    && prev.onCancelTool === next.onCancelTool
    && prev.isThinking === next.isThinking
    && prev.thinkingContent === next.thinkingContent
    && prev.thinkingStartTime === next.thinkingStartTime
    && prev.isLive === next.isLive;
});

/** Pair tool_call with its corresponding tool_result.
 *
 * Pairing is keyed EXCLUSIVELY on `toolCallId`. The backend assigns a
 * unique `tool_call_id` per LLM-issued tool invocation at tool-dispatch
 * time (see `acowork-runtime` builtin tools) and stamps the SAME id on
 * the emitted `tool_call` record AND its `tool_result` record. This id
 * is stable across transport (MQTT live, JSONL history), across order
 * (parallel calls may arrive in any order), and across reconnection
 * (replay emits records with the same id). Using `toolName` for pairing
 * is unsafe: many calls share a name (`file_read`, `content_search`,
 * `shell`, `bash`) and out-of-order delivery then mismatches them, which
 * is what produced the "tool_call split into two" rendering bug.
 *
 * Fallback policy:
 *  - tool_call without toolCallId → rendered as a standalone call;
 *    any result with the same absent id cannot be matched — we leave
 *    both as visible siblings rather than silently mispair.
 *  - tool_result without toolCallId → orphan; rendered standalone.
 *
 * The pair loop walks items in order so that streaming pendings appear
 * in the natural arrival sequence; the matching itself is id-based, so
 * the result may legitimately appear AFTER its call (e.g. MQTT
 * reordering, slow tool execution).
 */
type PairedItem =
  | { kind: "thought"; msg: ChatMessage }
  | { kind: "tool"; call: ChatMessage; result?: ChatMessage }
  | { kind: "other"; msg: ChatMessage };

function buildPairedItems(items: ChatMessage[]): PairedItem[] {
  // Index all tool_results by toolCallId for O(1) lookup. Results with
  // no toolCallId cannot be paired and are kept as standalone orphans.
  const resultsById = new Map<string, ChatMessage>();
  const orphanResults: ChatMessage[] = [];
  for (const msg of items) {
    if (msg.type === "tool_result") {
      if (msg.toolCallId) resultsById.set(msg.toolCallId, msg);
      else orphanResults.push(msg);
    }
  }

  const consumedResults = new Set<string>();
  const paired: PairedItem[] = [];

  for (const msg of items) {
    if (msg.type === "thought") {
      paired.push({ kind: "thought", msg });
    } else if (msg.type === "tool_call") {
      let result: ChatMessage | undefined;
      if (msg.toolCallId) {
        const candidate = resultsById.get(msg.toolCallId);
        if (candidate && !consumedResults.has(candidate.id)) {
          consumedResults.add(candidate.id);
          result = candidate;
        }
      }
      paired.push({ kind: "tool", call: msg, result });
    } else if (msg.type === "tool_result") {
      // Skip if already consumed by a tool_call pairing via id.
      if (msg.toolCallId && consumedResults.has(msg.id)) continue;
      // Orphan (no toolCallId or duplicate id) — show standalone.
      paired.push({ kind: "tool", call: msg });
    } else {
      paired.push({ kind: "other", msg });
    }
  }

  // Tool_results with no toolCallId never landed in resultsById, so the
  // main loop above never consumed them. Append them at the end so the
  // order stays roughly sequential.
  for (const orphan of orphanResults) {
    paired.push({ kind: "tool", call: orphan });
  }

  return paired;
}

/** Render a paired item */
function PairedExploreItem({ item, isStreaming, pendingApproval, currentSessionId, onApprove, onCancelTool, toolProgress }: { item: PairedItem; isStreaming: boolean; pendingApproval?: Record<string, ToolApprovalNeededEvent> | null; currentSessionId?: string | null; onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void; onCancelTool?: (toolCallId: string) => void; toolProgress?: Record<string, { elapsedMs: number; timeoutMs: number }> }) {
  // Convergent model: content always comes from the frozen message.
  if (item.kind === "thought") {
    const content = item.msg.content;
    return (
      <ThinkBlock
        content={content}
        isStreaming={isStreaming && !item.msg.endTime}
        hasReplyStarted={false}
        startTime={item.msg.startTime}
        endTime={item.msg.endTime}
      />
    );
  }

  if (item.kind === "tool") {
    return <ToolCallItem call={item.call} result={item.result} pendingApproval={pendingApproval} currentSessionId={currentSessionId} onApprove={onApprove} onCancelTool={onCancelTool} toolProgress={toolProgress} />;
  }

  // Fallback
  return (
    <div className="text-text-tertiary " style={{ fontSize: EXPLORE_FONT_SIZE }}>
      {item.msg.content.slice(0, 120)}
    </div>
  );
}

/** Tool call + result paired display: icon + tool name + status indicator + expandable details */
function ToolCallItem({ call, result, pendingApproval, currentSessionId, onApprove, onCancelTool, toolProgress }: { call: ChatMessage; result?: ChatMessage; pendingApproval?: Record<string, ToolApprovalNeededEvent> | null; currentSessionId?: string | null; onApprove?: (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => void; onCancelTool?: (toolCallId: string) => void; toolProgress?: Record<string, { elapsedMs: number; timeoutMs: number }> }) {
  // ADR-045: local "cancelling" UI state for button feedback
  const [cancelling, setCancelling] = useState(false);

  // ADR-045: pull heartbeat entry for THIS tool. undefined = no heartbeat yet.
  // First heartbeat arrives 5s after tool starts → we stay in Phase A until then.
  const progressEntry = toolProgress?.[call.toolCallId ?? ""];
  const showProgress = progressEntry !== undefined;

  const handleCancel = useCallback(() => {
    // ADR-045 debug breadcrumb: surface the actual values to DevTools so we
    // can diagnose "button does nothing" reports. Safe to keep in dev —
    // negligible cost and users can always check the console.
    console.info("[ADR-045] handleCancel", {
      toolCallId: call.toolCallId,
      onCancelToolType: typeof onCancelTool,
      progressEntry,
      showProgress,
    });
    if (!call.toolCallId || cancelling || !onCancelTool) return;
    setCancelling(true);
    onCancelTool(call.toolCallId);
  }, [call.toolCallId, onCancelTool, cancelling, progressEntry, showProgress]);

  const { t } = useTranslation();
  const [showDetails, setShowDetails] = useState(false);
  const toolName = call.toolName ?? "tool";
  const isShell = isShellTool(toolName);
  const Icon = isShell ? Terminal : Wrench;

  // Determine status from result
  const isSuccess = result?.toolStatus === "success";
  const isError = result?.toolStatus === "error";
  const isPendingResult = !result;

  // Localized tool label: e.g. "Reading" while running, "Read" once done.
  // Falls back to the raw tool name (e.g. "file_read") if no translation is found,
  // so adding a new builtin tool doesn't immediately break the UI.
  const toolLabel = t(
    `tools.${toolName}.${isPendingResult ? "running" : "done"}`,
    { defaultValue: toolName },
  );

  // Check if this specific tool_call has a pending approval for the current session
  const specificApproval = pendingApproval && call.toolCallId ? pendingApproval[call.toolCallId] : undefined;
  const needsApproval = specificApproval
    ? approvalMatchesSession(specificApproval, currentSessionId) && isPendingResult
    : false;

  // Countdown timer for approval timeout
  const [remainingSecs, setRemainingSecs] = useState<number | null>(null);
  useEffect(() => {
    if (!needsApproval || !specificApproval?.approval_timeout_secs) {
      setRemainingSecs(null);
      return;
    }
    const total = specificApproval.approval_timeout_secs;
    setRemainingSecs(total);
    const interval = setInterval(() => {
      setRemainingSecs((prev) => {
        if (prev === null || prev <= 1) {
          clearInterval(interval);
          return 0;
        }
        return prev - 1;
      });
    }, 1000);
    return () => clearInterval(interval);
  }, [needsApproval, specificApproval?.approval_timeout_secs]);

  // Hide approval when countdown reaches 0 (Runtime auto-rejects)
  const showApproval = needsApproval && remainingSecs !== 0;
  const countdownLabel = remainingSecs !== null && remainingSecs > 0
    ? `${Math.floor(remainingSecs / 60)}:${String(remainingSecs % 60).padStart(2, "0")}`
    : remainingSecs === 0 ? "expired" : null;

  let summary = "";
  try {
    const params = JSON.parse(call.content || "{}");
    summary = summarizeToolCall(toolName, params, result, isShell);
  } catch {
    summary = call.content.slice(0, 60);
  }

  // Detect compressed tool result (replaced by context_recall placeholder)
  const isCompressed = result?.content?.startsWith("[Tool result compressed");

  return (
    <div className="min-w-0">
      <div
        className="flex min-w-0 w-full items-center gap-2 rounded-md bg-zinc-100 px-2.5 py-1.5 text-left transition-colors hover:bg-zinc-200 dark:bg-zinc-700/50 dark:hover:bg-zinc-700"
        style={{ fontSize: EXPLORE_FONT_SIZE }}
      >
        <button className="flex min-w-0 flex-1 items-center gap-2" onClick={() => setShowDetails(!showDetails)}>
          <Icon className="h-3.5 w-3.5 shrink-0 text-text-tertiary" />
          <span className="shrink-0 font-medium text-text-secondary ">{toolLabel}</span>
          {summary && (
            <span className="min-w-0 flex-1 truncate ml-1 text-left text-text-tertiary ">
              {summary}
            </span>
          )}
          {showProgress && progressEntry && call.toolCallId && (
            <div className="ml-2 flex items-center gap-2 text-10 text-text-tertiary ">
              <div className="relative h-0.5 w-12 overflow-hidden rounded-full bg-zinc-200 dark:bg-zinc-700">
                <div
                  className="absolute inset-y-0 left-0 bg-amber-500 dark:bg-amber-400 transition-[width] duration-500"
                  style={{ width: `${Math.min(100, (progressEntry.elapsedMs / progressEntry.timeoutMs) * 100).toFixed(1)}%` }}
                />
              </div>
              <span className="tabular-nums">{formatDuration(progressEntry.elapsedMs)}</span>
              {onCancelTool && (
                <button
                  onClick={handleCancel}
                  disabled={cancelling}
                  className="ml-1 text-text-tertiary hover:text-red-500 transition-colors disabled:opacity-30"
                  title={t("exploreBlock.cancelTool")}
                  aria-label={t("exploreBlock.cancelTool")}
                >
                  <Square className="h-4 w-4 fill-current" />
                </button>
              )}
            </div>
          )}
          {isCompressed && (
            <span className="shrink-0 rounded bg-amber-100 px-1.5 py-0.5 text-10 font-medium text-amber-700 dark:bg-amber-900/40 dark:text-amber-400">
              已压缩
            </span>
          )}
        </button>
        {/* Approval buttons — shown when this tool needs user approval */}
        {showApproval && onApprove && specificApproval && (
          <div className="flex items-center gap-1 shrink-0" onClick={(e) => e.stopPropagation()}>
            {countdownLabel && countdownLabel !== "expired" && (
              <span className="text-10 font-mono text-amber-600 dark:text-amber-400 shrink-0 min-w-[2.5rem] text-right">
                {countdownLabel}
              </span>
            )}
            <button
              onClick={() => onApprove("deny", specificApproval)}
              className="rounded-md border border-zinc-300 px-2 py-0.5 text-11 font-medium text-text-secondary transition-colors hover:bg-zinc-200 dark:border-zinc-500  dark:hover:bg-zinc-600"
            >
              Deny
            </button>
            <button
              onClick={() => onApprove("allow", specificApproval)}
              className="rounded-md px-2 py-0.5 text-11 font-medium text-white transition-opacity hover:opacity-90"
              style={{ backgroundColor: "var(--color-accent-strong)" }}
            >
              Allow
            </button>
          </div>
        )}
        {/* Expired indicator */}
        {needsApproval && remainingSecs === 0 && (
          <span className="text-10 text-red-500 dark:text-red-400 shrink-0">
            Timed out
          </span>
        )}
        {/* Status indicator */}
        {isSuccess ? (
          <Check className="h-3 w-3 shrink-0" style={{ color: "var(--color-accent)" }} />
        ) : isError && /^Error: Cancelled by user/.test(result.content) ? (
          // ADR-045: cancellation reuses the X glyph (consistent with other
          // "non-success" terminations) but is amber, not red, to signal that
          // the user caused it — distinct from a tool error.
          <X
            className="h-3 w-3 shrink-0 text-amber-500"
            aria-label={t("exploreBlock.cancelledByUser")}
          />
        ) : isError ? (
          <X className="h-3 w-3 shrink-0 text-red-500" />
        ) : isPendingResult && !showProgress && !showApproval ? (
          <span className="h-3 w-3 shrink-0 animate-pulse rounded-full bg-zinc-300 dark:bg-zinc-500" />
        ) : null}
        <button onClick={() => setShowDetails(!showDetails)}>
          {showDetails ? (
            <ChevronDown className="h-3 w-3 shrink-0 text-text-tertiary" />
          ) : (
            <ChevronRight className="h-3 w-3 shrink-0 text-text-tertiary" />
          )}
        </button>
      </div>
      {showDetails && (
        <div className="mt-0.5 ml-5 space-y-0.5">
          {/* Call params */}
          <pre className="rounded bg-zinc-100 p-2 text-text-secondary dark:bg-zinc-800  whitespace-pre-wrap break-all" style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
            {call.content}
          </pre>
          {/* Result */}
          {result && (
            <pre className={`rounded p-2 whitespace-pre-wrap break-all ${isError ? "bg-red-50 text-red-600 dark:bg-red-900/20 dark:text-red-400" : "bg-[var(--color-accent)]/10 text-text-secondary dark:bg-[var(--color-accent)]/10 "}`} style={{ fontSize: EXPLORE_DETAIL_FONT_SIZE }}>
              {/* ADR-035 D9.2: backend already truncates tool_result to first 5 lines
                  in ALL paths (MQTT + HTTP); frontend does NOT re-truncate. */}
              {result.content}
            </pre>
          )}
        </div>
      )}
    </div>
  );
}