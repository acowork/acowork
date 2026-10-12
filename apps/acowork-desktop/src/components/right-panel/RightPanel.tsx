import { useState, useEffect, useRef, useCallback } from "react";
import { useTranslation } from "../../i18n/useTranslation";
import { invoke } from "@tauri-apps/api/core";
import { useChatStore } from "../../stores/chatStore";
import { useAgentStore } from "../../stores/agentStore";
import { useDebugStore } from "../../stores/debugStore";
import { useSettingsStore } from "../../stores/settingsStore";
import type { ChatMessage, ProviderAccount, SessionStatus } from "../../lib/types";
import { getProcessingPhase } from "../../lib/types";
import { cn, formatPercent } from "../../lib/utils";
import { log } from "../../lib/logger";
import { computeCacheHitStats, formatCacheHitRate, hasCacheData, getCacheProtocol } from "../../lib/cacheHitRate";
import {
  WifiOff,
  Play,
  Pause,
  StepForward,
  Square,
  RefreshCw,
  RotateCcw,
  Bug,
  ChevronLeft,
  ChevronRight,
} from "lucide-react";
import { AgentSetupTab } from "./AgentSetupTab";
import { ToolsTab } from "./ToolsTab";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { MemoryPanel } from "../memory/MemoryPanel";
import { WorkspaceExplorer } from "../workspace/WorkspaceExplorer";
import { ControlButton, StateLabel, SnapshotNode } from "../debug/DebugPanel";
import { CompressionHistoryCard } from "../debug/CompressionHistoryCard";
// ADR-063 §3.7 — package prompt override editor. Always rendered at
// the TOP of the Debug tab regardless of DevMode state (operators can
// prepare overrides before clicking "Enter Debug"; the reload button
// stays reachable once DevMode is on).
import { PromptList } from "../debug/PromptList";
import { ListBox, ExpandableRow } from "../common/list";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { Switch } from "../common/Switch";
import { getGatewayUrl } from "../../lib/config";

interface RightPanelProps {
  onCollapse: () => void;
  isDebugMode?: boolean;
  onResizeStart?: (e: React.MouseEvent) => void;
  activeTab: "debug" | "status" | "setup" | "tools" | "memory" | "workspace";
  onTabChange: (tab: "debug" | "status" | "setup" | "tools" | "memory" | "workspace") => void;
}

// Stable empty array reference to avoid Zustand selector infinite loop
const EMPTY_MESSAGES: ChatMessage[] = [];

// Monaco-style top-edge shadow for the right panel: the tab content area
// shows a soft gradient under the title bar's hairline whenever the active
// tab's primary scroll container is scrolled away from the very top. Dead-
// zone threshold keeps the shadow from flickering on tiny scrollTop jitter.
const RIGHT_PANEL_TOP_SHADOW_THRESHOLD_PX = 4;

export function RightPanel({ width, isDebugMode = false, onResizeStart, activeTab, onTabChange }: RightPanelProps & { width: number }) {
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => s.selectedAgentId ? s.agents[s.selectedAgentId]?.meta : undefined);
  // Design doc 24 §8.2 (F7): the debug RPC tunnels through the Gateway
  // `/api/debug/*` endpoints, which the relay's remote guard 404s on
  // purpose. DevMode stays a same-machine / LAN feature — in relay mode
  // the toggle is disabled so the user isn't invited to enable a
  // session that could never attach.
  const debugRemoteUnavailable = useSettingsStore((s) => s.gatewayMode === "relay");
  const activeSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  const tokenUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.tokenUsage ?? null;
  });
  const contextUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.contextUsage ?? null;
  });
  // Regression t-83afab47: agent-scoped cumulative token totals come
  // exclusively from `contextUsage.agent_total_*`, which every
  // `session_state` / `ChunkEvent::ContextUsage` push carries via the
  // Runtime's `patch_agent_totals` helper. No client-side fallback —
  // when `contextUsage` has not yet reported, the rows render as
  // "—" so the operator can tell "no data yet" apart from "0".
  const sessionStatus: SessionStatus | null = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.sessionStatus ?? null;
  });
  const sessionModel = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.model ?? null;
  });
  const sessionProvider = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.provider ?? null;
  });
  // Multi-account pick: `providerAccountId` of the currently-active
  // session. ADR-012. The Runtime mirrors this onto the per-session
  // `sessionStates[sid].providerAccountId` field (MQTT session_config +
  // pull-repair from `GET /sessions/{sid}/config`), so this stays in
  // sync as the user switches apikeys from the input-box model menu.
  const sessionProviderAccountId = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.providerAccountId ?? null;
  });
  // Shared across the desktop — `loadModels` in ChatPanel populates
  // this map from `invoke('list_keys')`. Reading here avoids a second
  // IPC roundtrip; the list is refreshed whenever the input-box menu
  // opens, which is the same trigger that affects what the session-
  // status card should display.
  const providerAccounts = useChatStore((s) => s.providerAccounts);
  // Multi-account alias resolution for the right-panel session-status
  // card. Mirrors the gating the input-box model menu uses
  // (`accounts.length > 1`): only show an alias row when the provider
  // has more than one account — single-account providers don't need the
  // extra row, and showing it would clutter every existing session.
  const sessionProviderAlias = resolveSessionProviderAlias(
    sessionProvider,
    sessionProviderAccountId,
    providerAccounts,
  );
  // ADR-066 §6: only Anthropic-protocol providers surface
  // `cache_creation_input_tokens` (the cache-write counter); OpenAI
  // Chat Completions and OpenAI-compatible providers never do. We use
  // this to gate the agent-total cache-WRITE row in the Agent Status
  // panel below — for OpenAI sessions that value is hard-wired to 0
  // by the Runtime, so showing "0 累计缓存写入" next to a live cache
  // READ counter is misleading (the metric simply doesn't exist for
  // the chosen protocol). The cache-READ row is unconditional, since
  // both protocol families report it.
  const cacheProtocol = getCacheProtocol(sessionProvider);
  const modelRatio = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.ratio ?? null;
  });
  const reasoningEffort = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.reasoningEffort ?? null;
  });
  const temperature = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.temperature ?? null;
  });
  const openSessionCount = useChatStore((s) => {
    if (!selectedAgentId) return 0;
    const agent = s.agentStates[selectedAgentId];
    return agent?.openSessionIds?.length ?? 0;
  });
  // Historical session total for the agent — from the backend session list
  // scan (`total_count` in GET /api/agents/{id}/sessions), refreshed by
  // `agentStore.fetchSessions`. Distinct from `openSessionCount` (active tabs).
  const totalSessionCount = useAgentStore((s) => {
    if (!selectedAgentId) return 0;
    return s.agents[selectedAgentId]?.pagination.totalCount ?? 0;
  });
  const isCompacting = useChatStore((s) => {
    if (!selectedAgentId) return false;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return false;
    return agent.sessionStates[agent.activeSessionId]?.isCompacting ?? false;
  });
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return EMPTY_MESSAGES;
    return agent.sessionStates[agent.activeSessionId]?.messages ?? EMPTY_MESSAGES;
  });

  // ADR-066 §6: cache-hit ratio.  Provider billing model differs:
  //   - Anthropic Messages: `cache_read / (input + cache_read + cache_write)`
  //     (write is a real billable event — the denominator includes it).
  //   - OpenAI Chat Completions: `cache_read / prompt_tokens`
  //     (OpenAI has no write concept; auto-cache only).
  //   - Other providers (ollama, deepseek, zhipuai, …): no cache
  //     accounting at all → helper returns `null` and we hide the row.
  // This block is the **session-status** surface, so it shows the
  // session-lifetime (cumulative) rate — the input-box popover shows
  // the per-turn rate instead.  We deliberately do NOT compute this on
  // the backend — the choice of denominator is a UX policy owned by
  // the front-end so designers can tweak it without re-shipping the
  // runtime.
  const cacheStats = computeCacheHitStats(sessionProvider, contextUsage, "cumulative");
  const cacheHitRateLabel = formatCacheHitRate(cacheStats.ratio);
  // Numeric 0-100 for the progress-bar width (mirrors `usage_percent`).
  const cacheHitRatePercent =
    cacheStats.ratio != null ? Math.round(cacheStats.ratio * 100) : 0;

  // ── Debug store (always called, conditionally used) ──────────────
  const {
    connected,
    debugAgentId,
    sessionStates,
    connect,
    disconnect,
    disableDebugMode,
    resume,
    pause: pauseDebug,
    step,
    stop,
    restart,
    getSection,
    rewind,
    reExecute,
    patchContext,
  } = useDebugStore();
  const { t } = useTranslation();
  const sessionDebugState = activeSessionId ? sessionStates[activeSessionId] : null;
  const iteration = sessionDebugState?.iteration ?? 0;
  const phase = sessionDebugState?.phase ?? "Idle";
  const debugState = sessionDebugState?.debugState ?? "Stepping";
  const promptTokens = sessionDebugState?.promptTokens ?? 0;
  const completionTokens = sessionDebugState?.completionTokens ?? 0;
  const snapshots = sessionDebugState?.snapshots ?? [];
  const sectionCache = sessionDebugState?.sectionCache ?? new Map();
  const hasPendingPatches = sessionDebugState?.hasPendingPatches ?? false;
  const autoConnectAttempted = useRef(false);
  const prevAgentId = useRef<string | null>(null);

  // ── Monaco-style top-edge shadow ───────────────────────────────
  // Drives the gradient overlay under the title hairline. Unlike the chat
  // (single scroll container), each right-panel tab owns its own primary
  // scroll root (marked `data-tab-scroll`), some nested inside child
  // components, and they mount lazily per tab. We attach a native scroll
  // listener DIRECTLY to each marked root — a direct listener always fires
  // when that element scrolls (no reliance on capture propagation), and
  // because it is bound to the marked roots only, nested popups/dropdowns
  // can't flip the shadow. A MutationObserver keeps listeners attached
  // across tab switches and lazy-mounted tab content. Reset on tab switch
  // — the freshly shown tab starts at its top, and a stale `true` from the
  // previous tab would otherwise flash until the first scroll event.
  const [rightPanelScrolled, setRightPanelScrolled] = useState(false);
  const rpScrollAreaRef = useRef<HTMLDivElement>(null);
  useEffect(() => {
    const root = rpScrollAreaRef.current;
    if (!root) return;

    const attached = new WeakSet<Element>();
    const onMarkedScroll = (e: Event) => {
      const t = e.target as HTMLElement;
      setRightPanelScrolled(t.scrollTop > RIGHT_PANEL_TOP_SHADOW_THRESHOLD_PX);
    };
    const sync = () => {
      root.querySelectorAll<HTMLElement>("[data-tab-scroll]").forEach((el) => {
        if (!attached.has(el)) {
          el.addEventListener("scroll", onMarkedScroll);
          attached.add(el);
        }
      });
    };
    sync();
    const mo = new MutationObserver(sync);
    mo.observe(root, { childList: true, subtree: true });
    return () => {
      mo.disconnect();
      root.querySelectorAll("[data-tab-scroll]").forEach((el) => {
        if (attached.has(el)) {
          el.removeEventListener("scroll", onMarkedScroll);
          attached.delete(el);
        }
      });
    };
  }, []);
  useEffect(() => {
    setRightPanelScrolled(false);
  }, [activeTab]);


  // Debug section expansion / editing state
  const [expandedSections, setExpandedSections] = useState<Set<string>>(new Set());
  const [loadedSections, setLoadedSections] = useState<Set<string>>(new Set());
  // ADR-048 follow-up: in-flight flag for the runtime DevMode enable
  // button. Disables the button + changes its label while the
  // `enable_agent_debug` Tauri command is awaiting a response, so the
  // user can't double-click and double-fire the wiring.
  const [enablingDebug, setEnablingDebug] = useState(false);
  // Symmetric to `enablingDebug`: disables the "Exit Debug" button
  // while the runtime `disable_agent_debug` Tauri command +
  // `fetchAgents()` round-trip is in flight. The double-click
  // scenario matters here too — Runtime `disable_debug_mode` is
  // idempotent, but `fetchAgents()` would race with itself if the
  // user double-clicks.
  const [disablingDebug, setDisablingDebug] = useState(false);
  const [editingSection, setEditingSection] = useState<{
    iteration: number;
    section: string;
    original: string;
    current: string;
  } | null>(null);

  // Selected agent info (already derived above)

  // Iterations: 1-based per-session lifetime LLM-call count, sourced from
  // the Runtime-pushed `context_usage` event's `iteration` field (see
  // `ContextUsageInfo.iteration`), which is persisted in the session's
  // meta.json and therefore survives `Continue` after `max_iterations` and
  // Runtime restarts. Falls back to assistant-message count only when
  // running against a pre-iteration Runtime or before the first event
  // arrives.
  const iterations =
    contextUsage?.iteration ??
    messages.filter((m) => m.type === "assistant").length;

  // ── Debug auto-connect effect ────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode || !selectedAgentId) return;

    // ADR-048 D6: debug RPC goes through the Gateway HTTP reverse proxy
    // and debug events ride the shared MQTT subscription — the broker host
    // is derived from the Gateway URL (`derive_mqtt_broker_host` in
    // `commands/chat_mqtt.rs`), so remote mode (Gateway on a different
    // host) is fully supported. No local-only guard needed.
    const agentChanged = selectedAgentId !== prevAgentId.current;

    // ADR-048 follow-up: `debug_state === "enabled"` covers both
    // startup `--dev-mode` and runtime enable (dev_mode stays false
    // after POST /api/agents/{id}/debug/enable).
    if (selectedAgent?.debug_state === "enabled" && selectedAgent.alive) {
      if (agentChanged || !connected || debugAgentId !== selectedAgentId) {
        connect(selectedAgentId);
      }
      autoConnectAttempted.current = true;
    }

    if (agentChanged) {
      prevAgentId.current = selectedAgentId;
    }
  }, [isDebugMode, selectedAgentId, selectedAgent?.debug_state, selectedAgent?.alive, connected, debugAgentId, connect]);

  // ── Debug disconnect effect ──────────────────────────────────────
  useEffect(() => {
    if (!isDebugMode) return;
    if (connected && selectedAgent && (selectedAgent.debug_state !== "enabled" || !selectedAgent.alive)) {
      disconnect();
    }
  }, [isDebugMode, selectedAgent?.debug_state, selectedAgent?.alive, connected, disconnect]);

  // ── Debug toggle section callback ────────────────────────────────
  const toggleSection = useCallback(
    async (iteration: number, section: string) => {
      const key = `${iteration}:${section}`;
      setExpandedSections((prev) => {
        const next = new Set(prev);
        if (next.has(key)) {
          next.delete(key);
        } else {
          next.add(key);
          if (!loadedSections.has(key)) {
            getSection(activeSessionId, iteration, section);
            setLoadedSections((l) => new Set(l).add(key));
          }
        }
        return next;
      });
    },
    [activeSessionId, getSection, loadedSections]
  );

  // ── Switch to debug tab when entering debug mode ─────────────────
  const prevIsDebugMode = useRef(isDebugMode);
  useEffect(() => {
    if (isDebugMode && !prevIsDebugMode.current) {
      onTabChange("debug");
    }
    prevIsDebugMode.current = isDebugMode;
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [isDebugMode]);

  // ADR-034 Phase 5: Agent status query (fire-and-forget, no UI rendering yet).
  // Guarded on running+ready — the status endpoint proxies through the
  // Runtime and 503s against an unregistered one.  When the user starts the
  // agent, this selector re-fires the effect.
  useEffect(() => {
    if (!selectedAgentId) return;
    if (selectedAgent?.lifecycle !== "sessions_ready") return; // ADR-085: capability gate
    fetch(`${getGatewayUrl()}/api/agents/${selectedAgentId}/status`)
      .then((r) => r.ok ? r.json() : null)
      .then((data) => {
        if (data) log.debug("[RightPanel] Agent status:", data);
      })
      .catch(() => {/* ignore */});
  }, [selectedAgentId, selectedAgent?.alive, selectedAgent?.lifecycle]);

  // Context-snapshots level-1 collapse (whole card body toggles from the
  // card header — same interaction as the PROMPT card). Default open.
  const [snapshotsOpen, setSnapshotsOpen] = useState(true);
  // Status-tab level-1 collapsible cards (Session Status / Agent
  // Status) — same grammar as the Tools-tab "Builtin Tools" list:
  // the header row toggles the whole body; default open.
  const [sessionStatusOpen, setSessionStatusOpen] = useState(true);
  const [agentStatusOpen, setAgentStatusOpen] = useState(true);
  // Context snapshots are paged (like the memory list) so a long session
  // with dozens of iterations cannot push the compression-history card
  // out of the visible area. Page controls live in the empty right side
  // of the card header.
  const [snapshotPage, setSnapshotPage] = useState(0);
  const SNAPSHOT_PAGE_SIZE = 20;
  const snapshotTotalPages = Math.max(1, Math.ceil(snapshots.length / SNAPSHOT_PAGE_SIZE));
  const snapshotStart = snapshotPage * SNAPSHOT_PAGE_SIZE;
  const pageSnapshots = snapshots.slice(snapshotStart, snapshotStart + SNAPSHOT_PAGE_SIZE);
  // Clamp back to the last page when the list shrinks (session switch /
  // reload) and the current page no longer exists.
  useEffect(() => {
    if (snapshotPage >= snapshotTotalPages) {
      setSnapshotPage(Math.max(0, snapshotTotalPages - 1));
    }
  }, [snapshotPage, snapshotTotalPages]);

  return (
    <div className="relative flex flex-col shrink-0 ml-1" style={{ width }}>
      {/* Resize handle overlay — sits at the left edge */}
      <div
        className="absolute -left-1 top-0 bottom-0 w-1 cursor-col-resize z-10 group"
        onMouseDown={onResizeStart}
      >
        <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors" />
      </div>
      {/* Clip container — owns the rounded-xl surface + overflow-hidden so the
          full-bleed tab content (each tab root paints bg-right-panel) gets
          clipped to the rounded box: the bottom-left corner stays symmetric
          with the top-left. The resize handle sits ABOVE it (absolute at
          -left-1), so it must stay outside this clipping box or it would be
          cut off and resizing would break. */}
      <div className={cn(CAPSULE_PANE_CN, "flex-1 bg-right-panel")}>
      {/* Tab title header.
          Height MUST come from a fixed line box, not from px padding +
          the text line box: `pt-[10px] pb-[7px]` + a `text-xs` line meant
          the band grew with --ui-font-size (the line box scales), so this
          `border-b` drifted down at font sizes 1/3/4/5 and stopped lining
          up with the left column's header divider. `--tab-line-height`
          is the app's existing "tab strip keeps its height, text scales"
          contract (SessionTabBar / tab.tsx / FileEditorPanel tabs all
          use it); --ui-list-header-h is the nav-column value the left
          pane is pinned to. Belt and braces: both are px/rem, so the
          rule sits on the same baseline as the left column at every
          font size while the text itself still scales. */}
      <div className="flex min-h-[var(--ui-list-header-h)] shrink-0 items-center border-b border-border-divider px-[var(--tab-px)] text-[length:var(--tab-font-size)] font-medium leading-[var(--tab-line-height)] text-text-tertiary ">
        {t(`rightPanel.${activeTab}`)}
      </div>

      {/* Tab content area — relative wrapper owns the Monaco-style top-edge
          shadow overlay (driven by rightPanelScrolled) and the capture-phase
          scroll listener that feeds it. Each tab keeps its own flex-1 scroll
          root sizing unchanged. */}
      <div ref={rpScrollAreaRef} className="relative flex min-h-0 flex-1 flex-col">
        {rightPanelScrolled && (
          <div
            aria-hidden="true"
            className="pointer-events-none absolute inset-x-0 top-0 z-10 h-2 bg-linear-to-b from-black/3 to-transparent dark:from-black/40"
          />
        )}

      {/* ── Debug tab content ─────────────────────────────────────── */}
      {/* Restructured: the debug tab always renders an action block
          (with a two-state Enter/Exit Debug button on the left and the
          session debug controls on the right, gated by DevMode). The
          4 DevMode branches now live BELOW the action block:
            1. agent not running            → "no agent in debug mode"
               (replaces the action block — no point showing Enter Debug
                for an agent that isn't running)
            2. running, DevMode off         → action block only
               (the Enter Debug button is the only interactive element;
                the 4 session controls + lower cards are hidden)
            3. running, DevMode on, remote  → remote-unavailable
            4. running, DevMode on, local,
               not connected                → "debug connection lost"
            5. running, DevMode on, local,
               connected                    → state + snapshots + prompts
                                              + compression history */}
      {activeTab === "debug" && (
        <OverlayScrollbar dataTabScroll className="min-h-0 flex flex-1 flex-col bg-right-panel">
          {/* ADR-063 §3.7 — package prompt override editor. Always
              visible at the TOP of the Debug tab, BEFORE the
              "no agent running" placeholder, the action block, and
              the debug-only state/snapshots/compression cards.
              Operators must be able to browse and prepare overrides
              regardless of agent running state or DevMode. */}
          <PromptList />

          {/* Divider — full panel-width hairline separating the PROMPT
              override area (above) from the debug controls (below). Matches
              the workspace/memory panel divider style. */}
          <div className="my-2 border-t border-border-divider" />

          {!selectedAgent?.alive ? (
            <div className="flex flex-1 flex-col items-center justify-center gap-3 p-6 text-sm text-text-tertiary ">
              <Bug className="h-5 w-5" />
              <span className="text-center">
                {t("rightPanel.noAgentDebug")}
              </span>
            </div>
          ) : (
            <div className="p-3 space-y-3">
              {/* Action block — always shown when the agent is running.
                  Left: two-state text button (Enter/Exit Debug, btn-solid,
                  no icon). Right: 4 session debug buttons, only rendered
                  once DevMode is on.

                  Height contract — must render at the same total pixel
                  height as the PROMPT card header immediately above and
                  the level-1 collapsible cards below. Tailwind defaults
                  to `box-sizing: border-box`, so a banner that owns its
                  own border cannot use `min-h-[36px]` and expect to match
                  ExpandableRow's `min-h-[36px]` header — the border eats
                  2px and the banner ends up 2px shorter than every other
                  horizontal bar in the tab. The PROMPT card is
                  `ListBox (1+1 border) + ExpandableRow header (36px) =
                  38px` total, so this banner targets 38px to match.

                  Padding: `px-3` matches the ExpandableRow header so the
                  content's left/right inset reads identical across the
                  tab. The earlier `px-2` was 4px shy and made the
                  Switch + label sit visibly closer to the left edge
                  than the chevron + title of the cards below it. */}
              <div className="flex min-h-[38px] items-center rounded-md border border-border-outer bg-panel-block px-3">
                <div className="flex w-full items-center gap-1">
                  <Switch
                    checked={selectedAgent?.debug_state === "enabled"}
                    onChange={async (checked) => {
                      if (checked) {
                        /* Enter Debug — ADR-048 follow-up runtime DevMode
                           activation. Flips DevMode on at the Runtime via
                           POST /api/agents/{id}/debug/enable without
                           restarting the agent. Don't wait for the auto-
                           connect useEffect — its run is deferred to after
                           commit and would race the tab switch, leaving
                           `connected` momentarily false. Attach the debug
                           session synchronously. */
                        if (!selectedAgentId) return;
                        setEnablingDebug(true);
                        try {
                          log.debug("[RightPanel] enable_agent_debug: invoking", { agentId: selectedAgentId });
                          await invoke<{ enabled: boolean; already_enabled: boolean; debug_port: number }>(
                            "enable_agent_debug",
                            { agentId: selectedAgentId, debugPort: 0 },
                          );
                          log.debug("[RightPanel] enable_agent_debug: invoke ok, refreshing agents");
                          await useAgentStore.getState().fetchAgents();
                          log.debug("[RightPanel] enable_agent_debug: calling connect() directly");
                          useDebugStore.getState().connect(selectedAgentId);
                          onTabChange("debug");
                        } catch (err) {
                          log.error("[RightPanel] enable_agent_debug failed:", err);
                        } finally {
                          setEnablingDebug(false);
                        }
                      } else {
                        /* Exit Debug — tears down agent-wide DevMode via
                           useDebugStore.disableDebugMode(). No agent restart. */
                        if (disablingDebug) return;
                        setDisablingDebug(true);
                        try {
                          log.debug("[RightPanel] exit_debug: invoking disableDebugMode");
                          await disableDebugMode();
                          log.debug("[RightPanel] exit_debug: disableDebugMode ok");
                        } catch (err) {
                          log.error("[RightPanel] exit_debug failed:", err);
                        } finally {
                          setDisablingDebug(false);
                        }
                      }
                    }}
                    disabled={enablingDebug || disablingDebug || debugRemoteUnavailable}
                    size="sm"
                    label={
                      debugRemoteUnavailable
                        ? t("rightPanel.debugRemoteUnavailable")
                        : enablingDebug
                          ? t("rightPanel.enteringDebug")
                          : disablingDebug
                            ? t("rightPanel.exitingDebug")
                            : selectedAgent?.debug_state === "enabled"
                              ? t("rightPanel.buttonExitDebug")
                              : t("rightPanel.enterDebug")
                    }
                    labelPosition="right"
                  />
                  <div className="ml-auto" />
                  {selectedAgent?.debug_state === "enabled" && (
                    <>
                      <ControlButton
                        onClick={() => {
                          if (debugState === "Paused") void resume(activeSessionId);
                          else if (debugState === "Stopped") void restart(activeSessionId);
                          else void pauseDebug(activeSessionId);
                        }}
                        title={
                          debugState === "Paused"
                            ? "Resume (F5)"
                            : debugState === "Stopped"
                              ? t("rightPanel.buttonRestart")
                              : "Pause (F6)"
                        }
                        active={debugState === "Paused"}
                      >
                        {debugState === "Paused"
                          ? <Play className="h-3.5 w-3.5" />
                          : <Pause className="h-3.5 w-3.5" />
                        }
                      </ControlButton>
                      <ControlButton
                        onClick={() => step(activeSessionId, "iteration")}
                        title={t("rightPanel.buttonStep")}
                        disabled={debugState === "Stopped"}
                      >
                        <StepForward className="h-3.5 w-3.5" />
                      </ControlButton>
                      <ControlButton
                        onClick={() => stop(activeSessionId)}
                        title={t("rightPanel.buttonStop")}
                        disabled={debugState === "Stopped"}
                      >
                        <Square className="h-3.5 w-3.5" />
                      </ControlButton>
                      <ControlButton onClick={() => restart(activeSessionId)} title={t("rightPanel.buttonRestart")} disabled={!debugAgentId}>
                        <RefreshCw className="h-3.5 w-3.5" />
                      </ControlButton>
                      {hasPendingPatches && (
                        <>
                          <div className="mx-1 h-4 w-px bg-zinc-200 dark:bg-zinc-700" />
                          <ControlButton
                            onClick={() => reExecute(activeSessionId).catch(log.error)}
                            title={t("rightPanel.buttonReExecute")}
                            active
                          >
                            <RotateCcw className="h-3.5 w-3.5" />
                          </ControlButton>
                        </>
                      )}
                    </>
                  )}
                </div>
              </div>

              {/* Below action block — only when in debug mode. The
                  lower cards (state, snapshots, prompts, compression
                  history) are intentionally hidden until the operator
                  commits to a debug session. */}
              {selectedAgent?.debug_state === "enabled" && (
                !connected ? (
                  <div className="flex flex-col items-center justify-center gap-3 p-6 text-sm text-text-tertiary ">
                    <WifiOff className="h-5 w-5" />
                    <span className="text-center">
                      {t("rightPanel.debugConnectionLost")}
                    </span>
                  </div>
                ) : (
                  <>
                    {/* State card */}
                    <div className="rounded-md border border-border-outer bg-panel-block p-3">
                      <div className="grid grid-cols-2 gap-x-3 gap-y-1 text-xs">
                        <StateLabel label={t("rightPanel.iteration")} value={`#${iteration}`} />
                        <StateLabel label={t("rightPanel.phase")} value={phase} highlight />
                        <StateLabel label={t("rightPanel.tokens")} value={`${promptTokens + completionTokens}`} />
                        <StateLabel
                          label={t("rightPanel.sessionStatusLabel")}
                          value={debugState}
                          highlight={debugState !== "Running" && debugState !== "Stepping"}
                        />
                      </div>
                    </div>
                    {/* Context snapshots card — level-1 collapsible list:
                        the header toggles the whole snapshot list (same
                        interaction as the PROMPT card, default open);
                        snapshot rows are separated by the unified hairline
                        so each iteration reads as a clear list row. When
                        there is more than one page, the empty right side of
                        the header carries the page number + arrows. */}
                    <ListBox dividers={false}>
                      <ExpandableRow
                        open={snapshotsOpen}
                        onToggle={() => setSnapshotsOpen((v) => !v)}
                        title={t("rightPanel.contextSnapshots", { count: snapshots.length })}
                        ariaLabel={t("rightPanel.contextSnapshots", { count: snapshots.length })}
                        bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset"
                        trailing={
                          snapshotTotalPages > 1 ? (
                            <span
                              className="flex items-center gap-1"
                              onClick={(e) => e.stopPropagation()}
                            >
                              <button
                                type="button"
                                aria-label="Previous snapshot page"
                                disabled={snapshotPage === 0}
                                onClick={() => {
                                  setSnapshotsOpen(true);
                                  setSnapshotPage((p) => Math.max(0, p - 1));
                                }}
                                className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-600 disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-zinc-400  dark:hover:bg-zinc-700 dark:hover:text-zinc-300 dark:disabled:hover:bg-transparent dark:disabled:hover:text-zinc-500"
                              >
                                <ChevronLeft className="h-3.5 w-3.5" />
                              </button>
                              <span className="min-w-[3ch] text-center font-mono text-10 tabular-nums text-text-tertiary ">
                                {snapshotPage + 1}/{snapshotTotalPages}
                              </span>
                              <button
                                type="button"
                                aria-label="Next snapshot page"
                                disabled={snapshotPage >= snapshotTotalPages - 1}
                                onClick={() => {
                                  setSnapshotsOpen(true);
                                  setSnapshotPage((p) => Math.min(snapshotTotalPages - 1, p + 1));
                                }}
                                className="rounded p-1 text-text-tertiary transition-colors hover:bg-zinc-100 hover:text-zinc-600 disabled:opacity-30 disabled:hover:bg-transparent disabled:hover:text-zinc-400  dark:hover:bg-zinc-700 dark:hover:text-zinc-300 dark:disabled:hover:bg-transparent dark:disabled:hover:text-zinc-500"
                              >
                                <ChevronRight className="h-3.5 w-3.5" />
                              </button>
                            </span>
                          ) : undefined
                        }
                      >
                        {snapshots.length === 0 && (
                          <div className="px-3 py-3 text-center text-xs text-text-tertiary">
                            {t("rightPanel.noSnapshots")}
                            <br />
                            {t("rightPanel.sendMessageToGenerate")}
                          </div>
                        )}
                        {pageSnapshots.length > 0 && (
                          <ListBox variant="plain">
                            {pageSnapshots.map((snap) => (
                              <SnapshotNode
                                key={snap.iteration}
                                snapshot={snap}
                                expandedSections={expandedSections}
                                sectionCache={sectionCache}
                                editingSection={editingSection}
                                onToggleSection={(section) => toggleSection(snap.iteration, section)}
                                onStartEdit={(section, original) =>
                                  setEditingSection({ iteration: snap.iteration, section, original, current: original })
                                }
                                onCancelEdit={() => setEditingSection(null)}
                                onSaveEdit={(section, content) => {
                                  const patches: Record<string, unknown> = {};
                                  patches[section] = content;
                                  patchContext(activeSessionId, patches).catch(log.error);
                                  setEditingSection(null);
                                }}
                                onEditChange={(content) =>
                                  setEditingSection((prev) => (prev ? { ...prev, current: content } : null))
                                }
                                onRewind={(iter) => rewind(activeSessionId, iter).catch(log.error)}
                                getSection={(iteration, section) => getSection(activeSessionId, iteration, section)}
                                // Anchor per-section token counts to the real, LLM-billed
                                // total for the latest call so they sum exactly to
                                // the value the user sees in the context-usage popover
                                // instead of the per-section `token_estimate` heuristic.
                                //
                                // Only after the first real LLM call (`iteration` >= 1, the
                                // runtime's per-session LLM-call counter). Before then,
                                // `context_usage.total_tokens` is a history-only estimate
                                // (the runtime's `emit_session_state` uses `live_tokens`,
                                // which excludes the system prompt / tool defs), so scaling
                                // the assembled section byte-shares by it produces wildly
                                // small per-section numbers (a 3.4 KB system prompt shown
                                // as ~15 tok). Without the anchor, DebugPanel falls back to
                                // the self-consistent per-section `token_estimate`.
                                realTotalTokens={
                                  contextUsage && (contextUsage.iteration ?? 0) > 0
                                    ? contextUsage.total_tokens
                                    : undefined
                                }
                              />
                            ))}
                          </ListBox>
                        )}
                      </ExpandableRow>
                    </ListBox>
                    <CompressionHistoryCard
                      agentId={selectedAgentId}
                      sessionId={activeSessionId}
                    />
                  </>
                )
              )}
            </div>
          )}
        </OverlayScrollbar>
      )}

      {/* ── Status tab content ───────────────────────────────────── */}
      {activeTab === "status" && (
        <OverlayScrollbar dataTabScroll className="flex-1 bg-right-panel p-3">
          {/* Session Status — level-1 collapsible card matching the
              Tools-tab "Builtin Tools" grammar: the clickable header
              row (chevron + title) toggles the whole stats body, which
              sits on the inset surface below a hairline. Default open. */}
          <ListBox dividers={false}>
            <ExpandableRow
              open={sessionStatusOpen}
              onToggle={() => setSessionStatusOpen((v) => !v)}
              title={t("rightPanel.sessionStatus")}
              ariaLabel={t("rightPanel.sessionStatus")}
              bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-2 text-xs"
            >
              {/* Context usage progress bar */}
              {contextUsage ? (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-text-tertiary">{t("rightPanel.contextUsage")}</span>
                    <span className="font-mono font-medium" style={{ color: "var(--color-accent)" }}>
                      {formatPercent(contextUsage.usage_percent)}%
                    </span>
                  </div>
                  <div className="h-1.5 rounded-full bg-zinc-200 overflow-hidden dark:bg-zinc-700 mb-1.5">
                    <div
                      className="h-full rounded-full transition-all duration-300"
                      style={{ backgroundColor: "var(--color-accent)", width: `${contextUsage.usage_percent}%` }}
                    />
                  </div>
                  <div className="flex justify-between text-text-tertiary ">
                    <span>{formatTokenCount(contextUsage.total_tokens)} {t("rightPanel.used")}</span>
                    <span>{formatTokenCount(contextUsage.usable_context)} / {formatTokenCount(contextUsage.context_window)} {t("rightPanel.available")}</span>
                  </div>
                  {/* Compacting indicator */}
                  {isCompacting && (
                    <div className="flex items-center gap-1.5 mt-1">
                      <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                      <span className="thinking-shimmer text-text-tertiary">{t("rightPanel.compacting")}</span>
                    </div>
                  )}
                </div>
              ) : (
                <div className="mb-3 text-text-tertiary  italic">{t("rightPanel.noContextData")}</div>
              )}
              {/* ADR-066: cache hit ratio — same progress-bar style as the
                  context-usage block above.  This is the session-status
                  surface, so it shows the session-lifetime (cumulative)
                  rate.  Shown whenever the Runtime reports any cumulative
                  cache accounting (read or write); the ratio itself falls
                  back to a dash when it isn't computable yet (e.g. a fresh
                  Anthropic session that has only seeded the cache), in
                  which case the progress bar is hidden.  The two numbers
                  below are the ratio's numerator (cumulative cache-hit
                  tokens) and denominator (cumulative input tokens) — never
                  two independent cache counters. */}
              {hasCacheData(contextUsage, "cumulative") && (
                <div className="mb-3">
                  <div className="flex items-center justify-between mb-1">
                    <span className="text-text-tertiary">{t("rightPanel.cacheHitRatio")}</span>
                    <span className="font-mono font-medium" style={{ color: "var(--color-accent)" }}>
                      {cacheHitRateLabel ?? "\u2014"}
                    </span>
                  </div>
                  {cacheHitRateLabel !== null && (
                    <div className="h-1.5 rounded-full bg-zinc-200 overflow-hidden dark:bg-zinc-700 mb-1.5">
                      <div
                        className="h-full rounded-full transition-all duration-300"
                        style={{ backgroundColor: "var(--color-accent)", width: `${cacheHitRatePercent}%` }}
                      />
                    </div>
                  )}
                  <div className="flex justify-between text-text-tertiary ">
                    <span>{formatTokenCount(cacheStats.numerator)} {t("rightPanel.cached")}</span>
                    <span>{formatTokenCount(cacheStats.denominator)} {t("rightPanel.promptTokens")}</span>
                  </div>
                </div>
              )}
              {/* Divider */}
              {contextUsage && <div className="border-t border-border-divider mb-2" />}
              <StatRow label={t("rightPanel.promptTokens")} value={(tokenUsage?.prompt_tokens ?? contextUsage?.input_tokens)?.toLocaleString()} />
              <StatRow label={t("rightPanel.completionTokens")} value={(tokenUsage?.completion_tokens ?? contextUsage?.output_tokens)?.toLocaleString()} />
              {/* Cumulative session totals — sourced from SessionTokens via the
                  context_usage WebSocket event. Distinct from the per-turn
                  Prompt / Completion rows above (which use the `last_` value
                  from the most recent LLM call). Rendered only when the runtime
                  has reported at least one LLM call for this session. */}
              <StatRow
                label={t("rightPanel.totalInputTokens")}
                value={contextUsage?.total_input_tokens?.toLocaleString()}
              />
              <StatRow
                label={t("rightPanel.totalOutputTokens")}
                value={contextUsage?.total_output_tokens?.toLocaleString()}
              />
              {/* 字符/token — kept next to the token rows above so all
                  token-related counters read as one block. A divider
                  below separates this token cluster from the runtime
                  / model / status fields that follow. */}
              <StatRow label={t("rightPanel.labelCharactersPerToken")} value={modelRatio != null ? modelRatio.toFixed(2) : undefined} />
              {/* Speed / responsiveness — session-level means the Runtime
                  samples once per LLM response and persists in the session
                  meta, so a resumed session shows the same figures. The two
                  are sampled independently (a call can yield a latency
                  sample without a speed one), so each has its own sample
                  count behind it. `—` until the first measurement, never a
                  fake 0; the unit lives in the label, matching
                  `labelCharactersPerToken`. */}
              <StatRow
                label={t("rightPanel.labelAvgOutputSpeed")}
                value={contextUsage?.avg_tps != null ? contextUsage.avg_tps.toFixed(1) : undefined}
              />
              <StatRow
                label={t("rightPanel.labelAvgFirstTokenLatency")}
                value={
                  contextUsage?.avg_ttft_ms != null
                    ? Math.round(contextUsage.avg_ttft_ms).toString()
                    : undefined
                }
              />
              {/* Divider — separates the token-count cluster above from
                  the runtime / model / status fields below. Uses `my-2`
                  (not `mb-2`) so the gap above and below the line is
                  symmetric — `StatRow` already has `py-1`, so an
                  asymmetric `mb-2` makes the line look glued to the
                  text above and far from the text below. */}
              <div className="my-2 border-t border-border-divider" />
              <StatRow label={t("rightPanel.iterations")} value={iterations ? String(iterations) : undefined} />
              {sessionModel && (
                <StatRow label={t("rightPanel.labelModel")} value={sessionModel} />
              )}
              {sessionProvider && (
                <StatRow label={t("rightPanel.labelProvider")} value={sessionProvider} />
              )}
              {/* Multi-account alias row — only when the current provider
                  actually has more than one account. Single-account
                  providers hide this row so existing sessions don't gain
                  a redundant line. The alias is what the model-menu
                  highlights as the session's apikey pick. */}
              {sessionProviderAlias && (
                <StatRow
                  label={t("rightPanel.labelApiKeyAlias")}
                  value={sessionProviderAlias}
                />
              )}
              {reasoningEffort != null && (
                <StatRow label={t("rightPanel.labelThinkingLevel")} value={reasoningEffort.charAt(0).toUpperCase() + reasoningEffort.slice(1)} />
              )}
              <StatRow label={t("rightPanel.labelTemperature")} value={temperature != null ? temperature.toFixed(2) : undefined} />
              <div className="flex justify-between py-1">
                <span className="text-text-tertiary">{t("rightPanel.sessionStatusLabel")}</span>
                <span className="flex items-center gap-1.5 text-text-secondary ">
                  <span
                    className={cn(
                      "inline-block h-2 w-2 rounded-full",
                      // ADR-049: 6-state color map. Each phase gets its own
                      // visual cue so the operator can distinguish "waiting"
                      // (gray), "streaming" (accent), "tool_executing"
                      // (blue), "waiting_approval" (yellow), "paused"
                      // (amber), and "idle" / unknown (zinc).
                      getProcessingPhase(sessionStatus) === "streaming" && "bg-[var(--color-accent)]",
                      getProcessingPhase(sessionStatus) === "waiting" && "bg-zinc-400 dark:bg-zinc-500",
                      getProcessingPhase(sessionStatus) === "tool_executing" && "bg-blue-400",
                      getProcessingPhase(sessionStatus) === "waiting_approval" && "bg-yellow-400",
                      getProcessingPhase(sessionStatus) === "paused" && "bg-amber-400",
                      (getProcessingPhase(sessionStatus) === "idle" || !sessionStatus) && "bg-zinc-300 dark:bg-zinc-600",
                    )}
                  />
                  {sessionStatus ? sessionStatus.status.replace(/_/g, " ") : "\u2014"}
                </span>
              </div>
            </ExpandableRow>
          </ListBox>

          {/* Divider — full panel-width hairline separating the Session
              Status card (above) from the Agent Status card (below).
              Matches the workspace/memory panel divider style. */}
          <div className="-mx-3 my-2 border-t border-border-divider" />

          {/* Agent Status — level-1 collapsible card, same grammar as
              the Session Status card above. */}
          <ListBox dividers={false}>
            <ExpandableRow
              open={agentStatusOpen}
              onToggle={() => setAgentStatusOpen((v) => !v)}
              title={t("rightPanel.agentStatus")}
              ariaLabel={t("rightPanel.agentStatus")}
              bodyClassName="rounded-b-md border-t border-border-divider bg-panel-inset px-3 py-2 text-xs"
            >
              {selectedAgent ? (
                <>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.sessionStatusLabel")}</span>
                    <span className="flex items-center gap-1.5">
                      <span
                        className={cn(
                          "inline-block h-2 w-2 rounded-full",
                          selectedAgent.alive ? "bg-[var(--color-accent)]" : "bg-zinc-300 dark:bg-zinc-600",
                        )}
                      />
                      <span className="text-text-secondary ">
                        {selectedAgent.alive ? t("rightPanel.running") : t("rightPanel.stopped")}
                      </span>
                    </span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.agent")}</span>
                    <span className="text-text-secondary ">{selectedAgent.name}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.version")}</span>
                    <span className="text-text-secondary ">{selectedAgent.version}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.activeSessions")}</span>
                    <span className="text-text-secondary ">{openSessionCount}</span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.totalSessions")}</span>
                    <span className="text-text-secondary ">{totalSessionCount}</span>
                  </div>
                  {/* Divider — separates the identity / session-count
                      cluster above from the agent-scoped token totals
                      below. Mirrors the divider style used in the
                      Session Status panel above the prompt/completion
                      token rows. Uses `my-2` so the gap above and
                      below the line is symmetric (the surrounding rows
                      use `py-1`, so an asymmetric `mb-2` would make the
                      line look glued to the row above). */}
                  <div className="my-2 border-t border-border-divider" />
                  {/* ADR-028 / ADR-066: agent-scoped cumulative totals across every
                      LLM call made by this Runtime process for this
                      agent. These are agent-level (not session-level)
                      figures, so they live in the Agent Status panel
                      rather than the session context panel above.

                      Regression t-83afab47: single data source — the
                      live `contextUsage.agent_total_*` from the
                      Runtime's `patch_agent_totals` helper. The
                      previous `agentStore.agents[id].agentTokenTotals`
                      fallback was removed because HTTP session-list
                      responses no longer carry `agent_total_*` fields
                      ("HTTP 拉完整数据, MQTT 推变化数据" rule), and
                      keeping a stale client-side fallback caused the
                      Agent Status panel to drift-flicker against the
                      live push on every `session_state` re-emit. When
                      `contextUsage` has not yet reported, the rows
                      render as "—" — distinct from "0". */}
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.agentTotalInputTokens")}</span>
                    <span className="font-mono text-text-secondary ">
                      {contextUsage?.agent_total_input_tokens?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.agentTotalOutputTokens")}</span>
                    <span className="font-mono text-text-secondary ">
                      {contextUsage?.agent_total_output_tokens?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  {/* ADR-066: agent-level cumulative cache totals. These are
                      agent-scoped (across every LLM call for this agent), so
                      they live in the Agent Status panel. The session-level
                      real-time cache hit rate lives in the Session Status
                      block above — it is NOT duplicated here. Same single-
                      data-source rule as the input/output rows above. */}
                  <div className="flex justify-between py-1">
                    <span className="text-text-tertiary">{t("rightPanel.agentTotalCacheReadTokens")}</span>
                    <span className="font-mono text-text-secondary ">
                      {contextUsage?.agent_total_cache_read_tokens?.toLocaleString() ?? "—"}
                    </span>
                  </div>
                  {/* ADR-066 §6: cache-write is an Anthropic-only concept.
                      OpenAI Chat Completions has no cache-write event
                      (caching is automatic and not surfaced as a per-
                      call write token), and the Runtime hard-wires this
                      counter to 0 for OpenAI-protocol providers. Showing
                      "0 累计缓存写入" next to a live cache-READ value
                      reads as a bug — better to hide the row entirely
                      for non-Anthropic sessions. */}
                  {cacheProtocol === "anthropic" && (
                    <div className="flex justify-between py-1">
                      <span className="text-text-tertiary">{t("rightPanel.agentTotalCacheWriteTokens")}</span>
                      <span className="font-mono text-text-secondary ">
                        {contextUsage?.agent_total_cache_write_tokens?.toLocaleString() ?? "—"}
                      </span>
                    </div>
                  )}
                </>
              ) : (
                <div className="py-1 text-text-tertiary ">{t("rightPanel.noAgentSelected")}</div>
              )}
            </ExpandableRow>
          </ListBox>
        </OverlayScrollbar>
      )}

      {/* ── Memory tab content — CSS visibility preserves component state across tab switches ── */}
      <div
        className="flex min-h-0 flex-1 flex-col overflow-hidden"
        style={{ display: activeTab === "memory" ? "flex" : "none" }}
      >
        <MemoryPanel />
      </div>

      {/* ── Setup tab content ─────────────────────────────────────── */}
      {activeTab === "setup" && (
        <div className="flex min-h-0 flex-1 flex-col overflow-hidden">
          <AgentSetupTab />
        </div>
      )}

      {/* ── Tools tab content ─────────────────────────────────────── */}
      {activeTab === "tools" && <ToolsTab />}

      {/* ── Workspace tab content ─────────────────────────────────── */}
      <div
        className="flex min-h-0 flex-1 flex-col overflow-hidden"
        style={{ display: activeTab === "workspace" ? "flex" : "none" }}
      >
        <WorkspaceExplorer />
      </div>
      </div>
      </div> {/* end: tab content area wrapper */}
    </div>

  );
}

function formatTokenCount(n: number): string {
  // M uses 2 decimals (= 10K granularity) so e.g. 2.71M and 2.78M
  // don't both collapse to "2.7M"; that was previously impossible
  // to distinguish in the cache-vs-input pair of the Session Status
  // panel because the gap between them was under 100K.
  // K keeps 1 decimal (100-token granularity) — at K scale that
  // resolution is still finer than any visual difference the eye
  // can pick out next to a M-scale neighbour, and bumping it would
  // just widen the column without adding real signal.
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(2)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}

function StatRow({ label, value }: { label: string; value?: string }) {
  return (
    <div className="flex items-center justify-between gap-2 py-1">
      <span className="shrink-0 text-text-tertiary">{label}</span>
      <span
        className="min-w-0 truncate font-mono text-text-secondary "
        title={value}
      >
        {value ?? "\u2014"}
      </span>
    </div>
  );
}

/**
 * Resolve the apikey alias to show in the right-panel Session Status
 * card for the active session. Returns `null` when:
 *   - the session has no provider or no account pick yet
 *   - the provider has fewer than two accounts (single-account
 *     providers don't need the row)
 *   - the picked account_id isn't in the vault (e.g. it was deleted
 *     after the session started; UI degrades silently rather than
 *     surfacing a stale alias)
 *
 * Mirrors the gating the input-box model menu uses
 * (`accounts.length > 1`): an alias is only worth showing when the
 * operator has a choice to make. Exported (not just `function`) so the
 * unit test can exercise every branch without spinning up the full
 * panel.
 */
export function resolveSessionProviderAlias(
  provider: string | null,
  providerAccountId: string | null,
  providerAccounts: Record<string, ProviderAccount[]>,
): string | null {
  if (!provider || !providerAccountId) return null;
  const accounts = providerAccounts[provider] ?? [];
  if (accounts.length <= 1) return null;
  return accounts.find((a) => a.accountId === providerAccountId)?.alias ?? null;
}
