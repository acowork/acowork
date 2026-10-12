import React, { useEffect, useInsertionEffect, useLayoutEffect, useRef, useState, useCallback, useMemo } from "react";
import { invoke, convertFileSrc } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore, type ConnectionStatus, STUCK_FORCE_RECONNECT_MS } from "../../stores/chatStore";
import { useGatewayStore } from "../../stores/gatewayStore";
import { useSkillStore } from "../../stores/skillStore";
import { useUserProfileStore } from "../../stores/userProfileStore";
import { useTranslation } from "../../i18n/useTranslation";
import type { ToolApprovalNeededEvent, AttachedItem } from "../../lib/types";
import { isProcessing, getProcessingPhase } from "../../lib/types";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { fetchProviderModels } from "../../lib/gateway-api";
import { startAgentAndSyncUI } from "../../lib/agent-start";
import { blobToBase64, extensionForMime } from "./clipboardImageExt";
import { listRowHover, menuItemHover, menuItemSelected, toolbarIconButton } from "../../lib/ui-styles";
import { AddProviderFlow } from "../harness/AddProviderFlow";
import * as sessionControl from "../../lib/session-control";
import { useActiveSessionReadOnly } from "../../lib/session-write-access";
import { Bot, Play, Send, ChevronDown, ChevronRight, ChevronLeft, ChevronsDown, ChevronsUp, Wrench, AlertTriangle, X, Square, Plus, Layers, Loader, Pencil, Paperclip, Image, Brain, Circle, CircleDot, Clipboard, Upload, Check, Search, Clock } from "lucide-react";
import type { ChatMessage, VaultKeyEntry, ModelEntry, ProviderAccount } from "../../lib/types";
import { ContextUsageIcon } from "./ContextUsageIcon";
import { SessionVisibilityToggle } from "./SessionVisibilityToggle";
import { blockIndexOfRawMessage } from "./messageFolder";
import { PlaceholderBar } from "./PlaceholderBar";
import { useSessionScope } from "./useSessionScope";
import { VirtualMessageList, type VirtualMessageListHandle } from "./VirtualMessageList";
import { OverlayScrollbar } from "../common/OverlayScrollbar";
import { useLiveStream, getChatAdapterSession, releaseAdapterSession } from "./chatAdapterStore";
import { useChatListAdapter } from "./chatListAdapter";
import { useScrollController } from "./useScrollController";
import { ContextMenu, useContextMenu } from "../common/ContextMenu";
import { ProviderLogo } from "../common/ProviderLogo";
import { useToast } from "../common/ToastProvider";

/**
 * Measure natural dimensions of an image whose src is already settable in the
 * DOM (data URL, asset protocol URL, blob URL). Returns the naturalWidth /
 * naturalHeight that the browser exposes once the image has loaded. ADR-046
 * uses these to populate `image_upload.width` / `.height` so the runtime
 * doesn't have to re-decode the blob just to render a thumbnail.
 */
function measureImage(src: string): Promise<{ width: number; height: number }> {
  return new Promise((resolve, reject) => {
    const img = new window.Image();
    img.onload = () =>
      resolve({ width: img.naturalWidth, height: img.naturalHeight });
    img.onerror = () => reject(new Error("Failed to load image for dimension detection"));
    img.src = src;
  });
}

/**
 * Merge an internal ref (used for click-outside detection) with an external
 * ref (used by the parent for layout measurement). Both refs are kept in sync
 * on every commit.
 */
function useMergedRef<T>(
    internalRef: React.RefObject<T | null>,
    externalRef?: React.Ref<T | null>,
): React.RefCallback<T | null> {
    return useCallback(
        (el: T | null) => {
            internalRef.current = el;
            if (typeof externalRef === "function") {
                externalRef(el);
            } else if (externalRef && typeof externalRef === "object") {
                (externalRef as React.MutableRefObject<T | null>).current = el;
            }
        },
        [internalRef, externalRef],
    );
}

import { AskQuestionCard } from "./AskQuestionCard";
import { DebugPausedBanner } from "./DebugPausedBanner";
import { RetryWaitBanner } from "./RetryWaitBanner";
import { SessionTabBar } from "./SessionTabBar";
import { SkillsPanel } from "../skills/SkillsPanel";
import { WorkspaceSelector } from "../workspace/WorkspaceSelector";
import { DocumentChip } from "./DocumentChip";
import { AttachedContextChips } from "./AttachedContextChips";
import { ToolbarDropdownTrigger } from "../common/ToolbarDropdown";
import { Tooltip } from "../common/Tooltip";
import { log } from "../../lib/logger";

// CHAT_BOTTOM_THRESHOLD_PX removed - pinned-to-bottom detection is now
// owned by useScrollController's state machine (PIN_THRESHOLD_PX).

// Stable empty array reference for the `messages` Zustand selector.
// Returning `[]` literals from a selector creates a new reference on every
// `getSnapshot` call, which trips useSyncExternalStore's "The result of
// getSnapshot should be cached" check and produces a "Maximum update depth
// exceeded" infinite re-render loop during transient states (mount, agent
// switch, session switch) where the agent's session entry does not yet
// exist. The same pattern is already used in RightPanel.tsx.
const EMPTY_MESSAGES: ChatMessage[] = [];

// Shared shell used by every pre-session placeholder (no-agents / no-selection
// / agent-stopped / session-loading) AND by the live chat view itself.
// Keeping the shape, border, and bg identical eliminates a visible flash on
// agent switch or app cold-start: the placeholder occupies the same rounded
// panel slot as the chat view, so when the session finally hydrates there is
// no transparent gap to "fill in". Mirrors the file-editor suspense shell in
// AppLayout.tsx for the same reason.
const CHAT_PANEL_SHELL_CN = cn(CAPSULE_PANE_CN, "min-w-[338px] flex-1 bg-chat-body");

// Monaco-style top-edge shadow: the messages area shows a soft gradient
// under the session tab bar's hairline whenever it is scrolled away from
// the very top, so the boundary reads as a depth edge (same affordance as
// a scrolled code view in Monaco). Dead-zone threshold keeps the shadow
// from flickering on tiny scrollTop jitter from scroll anchoring.
const CHAT_TOP_SHADOW_THRESHOLD_PX = 4;

/**
 * Data-driven scroll snapshot.  Stored per session key when the user
 * navigates away, consumed by useScrollController on return.
 *
 *   - atBottom: true when the user was viewing the latest content
 *     (or the session was streaming).  On return, the controller
 *     calls scrollToBottom().
 *   - firstVisibleBlockId: content-derived blockId of the first
 *     visible block when the user left.  Used to restore the exact
 *     reading position via vml.scrollToBlockId().  Stable across
 *     session switches (survives VML remount, page reload, etc.).
 */
interface ChatScrollSnapshot {
  atBottom: boolean;
  firstVisibleBlockId: string | null;
  /** Adapter messageOffset at save time. Pagination cursor (data, not pixels). */
  messageOffset: number | null;
  /** Index of the first visible block in the adapter's blocks array at save time.
   *  Combined with messageOffset, approximates the absolute message index to
   *  locate the correct page on restore (merged cache may span multiple pages). */
  firstVisibleBlockIndex: number | null;
}

// Bounded LRU: cap snapshot entries to MAX_SCROLL_SNAPSHOTS so a long-lived
// session that toggles Settings/Harness/Docs many times can't grow this Map
// without bound.  Each snapshot is ~50 bytes; 64 entries ≈ 3.2 KB, negligible.
// Eviction uses Map insertion order (JS Map preserves insertion order), so
// the OLDEST snapshot is dropped when the cap is exceeded — fine because
// the user just toggled away from the settings nav-back use case which is
// the only consumer of these snapshots.
const MAX_SCROLL_SNAPSHOTS = 64;
const chatScrollSnapshots = new Map<string, ChatScrollSnapshot>();

function setScrollSnapshot(key: string, snapshot: ChatScrollSnapshot): void {
  chatScrollSnapshots.set(key, snapshot);
  if (chatScrollSnapshots.size > MAX_SCROLL_SNAPSHOTS) {
    const oldestKey = chatScrollSnapshots.keys().next().value;
    if (oldestKey !== undefined) chatScrollSnapshots.delete(oldestKey);
  }
}

/**
 * Monotonic counter for `pendingAttachedItems[].tempId`.
 *
 * Combined with `Date.now()` (see `nextAttachmentTempId`), this gives
 * unique keys even when multiple uploads fire within the same millisecond
 * — a pure `Date.now()` suffix collided when several files were uploaded
 * at once, producing React duplicate-key warnings. Module-scoped (not
 * React-state) because we want uniqueness across hot-reload boundaries
 * too, and the value is purely a transient key, never persisted.
 */
let pendingAttachmentSeq = 0;

/**
 * Frontend mirror of `core/acowork-runtime/src/usecases/attachment.rs::MAX_UPLOAD_BYTES`
 * (50 MiB). Used to short-circuit oversized uploads BEFORE any
 * network roundtrip — we read the on-disk size via the Tauri `stat`
 * command and emit a toast immediately when the file would be
 * rejected by the runtime's `AttachmentError::PayloadTooLarge` (HTTP
 * 413) anyway.
 *
 * Keep the two values in sync; the backend value is the source of
 * truth and is enforced at the runtime, so this number is purely a
 * UX optimization (instant feedback instead of waiting for the
 * multipart upload to complete first).
 */
const MAX_UPLOAD_BYTES = 50 * 1024 * 1024;

/** Human-readable size for toast messages (e.g. "47.3 MB"). */
function formatBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(2)} GB`;
}

/**
 * Pick the input-box placeholder from (effectiveConnection, activeSkill).
 * P0 of unified-diagnostics plan §3.1.2.
 *
 * The MQTT verdict is the single authority for connection gating: the
 * old `gatewayStatus` branch was removed because the HTTP probe is a
 * display-side signal, and a superseded probe's late failure must never
 * decide what the user sees about the real connection (2026-09-22
 * incident: the input box stayed "正在重新连接..." after MQTT healed).
 *
 *   - connecting          → "正在连接 Agent..."
 *   - reconnecting        → "正在重新连接..."
 *   - stale               → "连接状态异常，正在恢复..."
 *   - disconnected / idle → "Gateway 未连接"
 *   - connected           → normal "输入消息..." / "输入参数..."
 *
 * Note: `stale` is a transient verdict (≤ STALE_GRACE_MS by definition),
 * so users normally see "正在连接 Agent..." / "正在重新连接..." and only
 * briefly "连接状态异常，正在恢复..." before the watchdog auto-recovers.
 * The stale placeholder exists so users aren't left wondering during the
 * recovery window.
 */
type InputPlaceholderKey =
  | "inputParams"
  | "inputMessage"
  | "inputParamsConnecting"
  | "inputMessageConnecting"
  | "inputParamsReconnecting"
  | "inputMessageReconnecting"
  | "inputParamsStale"
  | "inputMessageStale"
  | "inputParamsQueueing"
  | "inputMessageQueueing"
  | "inputGatewayDisconnected";

function getInputPlaceholderKey(
  gatewayStatus: string,
  effective: ConnectionStatus,
  activeSkill: boolean,
  sending: boolean,
): InputPlaceholderKey {
  // Agent is busy — Enter will queue the message. Hint the user up-front
  // so they don't think Enter is a no-op while waiting for the stream.
  if (sending) return activeSkill ? "inputParamsQueueing" : "inputMessageQueueing";
  if (gatewayStatus !== "connected") return "inputGatewayDisconnected";
  switch (effective) {
    case "connecting":
      return activeSkill ? "inputParamsConnecting" : "inputMessageConnecting";
    case "reconnecting":
      return activeSkill ? "inputParamsReconnecting" : "inputMessageReconnecting";
    case "stale":
      return activeSkill ? "inputParamsStale" : "inputMessageStale";
    case "disconnected":
    case "idle":
      return "inputGatewayDisconnected";
    case "connected":
    default:
      return activeSkill ? "inputParams" : "inputMessage";
  }
}

/**
 * P0-7: Live status banner above the input area. Shows the current
 * `effectiveConnection` and a real-time countdown ("X seconds until
 * auto-reconnect") so the user has a visible signal that the system is
 * working — not just a static "Connecting..." label that could mean
 * anything from "about to finish" to "stuck forever".
 *
 * Renders ONLY when:
 *   - effectiveConnection is one of connecting / reconnecting / stale /
 *     disconnected / **idle** (was: no banner for idle — fixed so a
 *     token-less boot that left the Rust client slot at `None` becomes
 *     visible to the user; the input placeholder still shows "Gateway
 *     not connected" while the HTTP side is down, so we don't double-up).
 *   - NOT `connected` (no banner once things are working).
 *
 * The countdown is driven by the connecting-episode clock
 * (`staleSince`, see chatStore): it counts down to
 * `staleSince + STUCK_FORCE_RECONNECT_MS` — the moment the watchdog
 * fires `force_reconnect_mqtt`. It therefore exists ONLY while a
 * connecting episode is in flight. `disconnected` is a terminal state
 * (the Rust session will not reconnect on its own), so instead of a
 * bogus "0s until auto-reconnect" it offers the one meaningful action:
 * a manual reconnect (H-2 of the review). `idle` is also a terminal
 * state (the Rust client was never created) — `force_reconnect_mqtt`
 * errors out without one (chat_mqtt.rs:887), so this branch offers
 * `connect_mqtt` instead: it is idempotent when a client already
 * targets the same broker, and it bootstraps one when none exists. We
 * tick at 1 Hz, which is enough resolution for a human-friendly
 * display and avoids sub-second re-renders that would trigger Zustand
 * churn.
 */
function ConnectionStatusBanner({
  effectiveConnection,
  staleSince,
}: {
  effectiveConnection: ConnectionStatus;
  staleSince: number | null;
}): React.ReactElement | null {
  const { t } = useTranslation();
  const { addToast } = useToast();
  // H-2: `disconnected` never auto-recovers — offer the manual action
  // (the button is disabled while the invoke is in flight).
  const [reconnecting, setReconnecting] = useState(false);
  const handleReconnect = useCallback(async () => {
    setReconnecting(true);
    try {
      await invoke("force_reconnect_mqtt");
    } catch (err) {
      log.warn("[connection-banner] force_reconnect_mqtt failed:", err);
    } finally {
      setReconnecting(false);
    }
  }, []);
  // `idle` mirrors `force_reconnect_mqtt` semantics but targets the
  // bootstrap path: there is no Rust client to rebuild, so we ask
  // Tauri to create one (`connect_mqtt` is idempotent — see
  // chat_mqtt.rs:208-230). Same Idempotency holds once the client is
  // already live: returns immediately when the endpoint matches.
  const handleConnect = useCallback(async () => {
    setReconnecting(true);
    try {
      await invoke("connect_mqtt");
    } catch (err) {
      log.warn("[connection-banner] connect_mqtt failed:", err);
      // Used to die in the log — the button looked dead ("clicked it,
      // nothing happened"). Surface the reason (e.g. the relay+http
      // rejection) so the user can act instead of re-clicking forever.
      addToast({ type: "error", message: String(err) });
    } finally {
      setReconnecting(false);
    }
  }, [addToast]);
  // The banner speaks for the MQTT/agent-side liveness — the single
  // authority for connection UI. It no longer requires the HTTP `status`
  // to read "connected": that display-side signal must not decide what
  // the user is told about the real connection.
  const isBannerWorthy =
    effectiveConnection === "connecting" ||
    effectiveConnection === "reconnecting" ||
    effectiveConnection === "stale" ||
    effectiveConnection === "disconnected" ||
    effectiveConnection === "idle";

  // Tick once per second so the countdown animates. Cheap (single
  // setState/ChatPanel re-render, no global store updates).
  const [now, setNow] = useState<number>(Date.now());
  useEffect(() => {
    if (!isBannerWorthy) return;
    const id = window.setInterval(() => setNow(Date.now()), 1000);
    return () => window.clearInterval(id);
  }, [isBannerWorthy]);

  if (!isBannerWorthy) return null;

  const deadlineAt = staleSince !== null ? staleSince + STUCK_FORCE_RECONNECT_MS : null;
  const secondsRemaining =
    deadlineAt !== null ? Math.max(0, Math.ceil((deadlineAt - now) / 1000)) : null;

  // Per-state label + icon. `idle` shares the `disconnected` row in
  // the visual treatment (amber / AlertTriangle / manual action) — both
  // are terminal states where the Rust side needs the user's nudge —
  // but it gets its own i18n string so the message says "MQTT not
  // initialized" rather than "Agent disconnected" (the latter implies
  // we WERE connected and lost it; `idle` means we never connected).
  const stateLabel: Record<
    "connecting" | "reconnecting" | "stale" | "disconnected" | "idle",
    string
  > = {
    connecting: t("chatPanel.connectionBanner.connecting"),
    reconnecting: t("chatPanel.connectionBanner.reconnecting"),
    stale: t("chatPanel.connectionBanner.stale"),
    disconnected: t("chatPanel.connectionBanner.disconnected"),
    idle: t("chatPanel.connectionBanner.idle"),
  };
  const label = stateLabel[effectiveConnection as keyof typeof stateLabel] ?? "";

  // Visual severity drives the colour. `idle` joins `stale`/`disconnected`
  // as amber (warning); the rest are sky/blue (info). `idle` does not get
  // a countdown — the user clicks Connect, the watchdog does not auto-
  // recover an empty client slot.
  const isWarn =
    effectiveConnection === "stale" ||
    effectiveConnection === "disconnected" ||
    effectiveConnection === "idle";
  const containerCls = isWarn
    ? "border-amber-300/60 bg-amber-50 text-amber-900 dark:border-amber-700/60 dark:bg-amber-950/40 dark:text-amber-100"
    : "border-sky-300/60 bg-sky-50 text-sky-900 dark:border-sky-700/60 dark:bg-sky-950/40 dark:text-sky-100";
  const iconCls = isWarn ? "text-amber-500 dark:text-amber-300" : "text-sky-500 dark:text-sky-300";
  const retryCls = isWarn
    ? "text-amber-700 dark:text-amber-200 tabular-nums"
    : "text-sky-700 dark:text-sky-200 tabular-nums";

  const retryText =
    secondsRemaining === null
      ? null
      : secondsRemaining === 1
        ? t("chatPanel.connectionBanner.retryInOneSecond")
        : t("chatPanel.connectionBanner.retryIn", { seconds: secondsRemaining });

  return (
    <div
      role="status"
      aria-live="polite"
      data-testid="chat-connection-banner"
      className={cn(
        "mx-3 mt-2 flex items-center gap-2 rounded-md border px-3 py-2 text-xs",
        containerCls,
      )}
    >
      {isWarn ? (
        <AlertTriangle className={cn("h-3.5 w-3.5 flex-shrink-0", iconCls)} aria-hidden="true" />
      ) : (
        <Loader className={cn("h-3.5 w-3.5 flex-shrink-0 animate-spin", iconCls)} aria-hidden="true" />
      )}
      <span className="flex-1 truncate">{label}</span>
      {effectiveConnection === "disconnected" ? (
        <button
          type="button"
          data-testid="chat-connection-banner-reconnect"
          onClick={handleReconnect}
          disabled={reconnecting}
          className={cn(
            "flex-shrink-0 rounded border px-2 py-0.5 text-xs font-medium transition-colors",
            "border-amber-400/70 text-amber-800 hover:bg-amber-100",
            "dark:border-amber-600/70 dark:text-amber-200 dark:hover:bg-amber-900/40",
            reconnecting && "opacity-60",
          )}
        >
          {t("chatPanel.connectionBanner.reconnectNow")}
        </button>
      ) : effectiveConnection === "idle" ? (
        <button
          type="button"
          data-testid="chat-connection-banner-connect"
          onClick={handleConnect}
          disabled={reconnecting}
          className={cn(
            "flex-shrink-0 rounded border px-2 py-0.5 text-xs font-medium transition-colors",
            "border-amber-400/70 text-amber-800 hover:bg-amber-100",
            "dark:border-amber-600/70 dark:text-amber-200 dark:hover:bg-amber-900/40",
            reconnecting && "opacity-60",
          )}
        >
          {t("chatPanel.connectionBanner.connectNow")}
        </button>
      ) : retryText !== null ? (
        <span className={retryCls} data-testid="chat-connection-banner-countdown">
          {retryText}
        </span>
      ) : (
        <span className="text-text-tertiary ">
          {t("chatPanel.connectionBanner.recoveringHint")}
        </span>
      )}
    </div>
  );
}

export function ChatPanel() {
  const { t } = useTranslation();
  const { addToast } = useToast();
  const { selectedAgentId } = useAgentStore();
  const selectedAgent = useAgentStore((s) => selectedAgentId ? s.agents[selectedAgentId]?.meta : undefined);
  // Ponytail bug-fix (2026-09): button mash inside the 1-3s MQTT-online
  // window previously hit the Gateway "already running" branch and
  // surfaced a misleading error toast. The store's `startingAgentIds`
  // is the dedup gate; reading it reactively keeps the Play button in
  // a disabled spinner state until the start round-trip settles.
  const isStarting = useAgentStore((s) =>
    selectedAgentId ? s.startingAgentIds.has(selectedAgentId) : false,
  );

  // ── Chat-top scroll shadow ─────────────────────────────────────
  // Monaco-style edge shadow (see CHAT_TOP_SHADOW_THRESHOLD_PX above).
  // Drives a gradient overlay right under the session tab bar. Updated in
  // the messages container's onScroll; setting the same boolean value is
  // a no-op re-render, so updating it on every scroll event is cheap.
  const [chatScrolledFromTop, setChatScrolledFromTop] = useState(false);

  // ── Toolbar responsive collapse ──────────────────────────────────
  // The bottom toolbar (model / think / workspace / skills + upload buttons)
  // must keep all buttons non-overlapping when the panel width narrows.
  // We measure each button's full width vs. its icon-only width and greedily
  // fold labels — starting from the leftmost button (model) and moving
  // rightward (effort → ws → sk) — until the row fits.
  const toolbarRef = useRef<HTMLDivElement>(null);
  // Per-button refs kept ONLY for the inner Menu components' click-outside
  // detection (ModelMenu/ReasoningEffortMenu use useMergedRef internally).
  // The toolbar collapse measurement no longer reads these — it queries
  // the DOM via [data-toolbar-btn] for a timing-independent answer.
  const modelBtnRef = useRef<HTMLDivElement>(null);
  const effortBtnRef = useRef<HTMLDivElement>(null);
  const wsBtnRef = useRef<HTMLDivElement>(null);
  const skBtnRef = useRef<HTMLDivElement>(null);
  const [textHidden, setTextHidden] = useState<Record<string, boolean>>({
    model: false,
    effort: false,
    ws: false,
    sk: false,
  });

  // ── Toolbar responsive collapse ───────────────────────────────
  //
  // IMPLEMENTATION: a `ref` callback (NOT a useEffect on toolbarRef).
  //
  // Why ref callback instead of useEffect?
  //
  // ModelMenu and ReasoningEffortMenu are conditionally rendered:
  //   - ModelMenu:     `availableModels.length > 0 && selectedAgent?.alive`
  //   - ReasoningMenu: `selectedAgent?.alive && currentReasoningEffort != null`
  //
  // On cold start the toolbar div itself may render before its
  // conditionally-rendered children, or vice versa, depending on the
  // order of state hydration.  useEffect with `[]` runs once on mount
  // and then never again — so if it runs BEFORE the model/effort buttons
  // exist, ResizeObserver is attached to a half-formed DOM and stays
  // attached even when the buttons arrive later, BUT the observer's
  // initial synchronous callback already fired with the half-formed
  // state and cached "no buttons → no fold" forever.
  //
  // A ref callback fires every time React attaches the ref to a DOM
  // node — and crucially, when the toolbar div re-mounts due to React
  // reconciler activity (StrictMode double-invoke, or any parent state
  // that causes ChatPanel's children to re-render and remount the
  // toolbar subtree), the OLD ref is called with `null` (cleanup) and
  // the NEW ref is called with the new node (setup).  This naturally
  // tears down and re-installs the observer across all "toolbar
  // identity" changes, which is exactly what we need.
  //
  // Inside the ref callback we ALSO install a MutationObserver on the
  // toolbar subtree to catch button-set changes that don't cause the
  // toolbar itself to remount (agent becomes running → model menu
  // appears inside the toolbar).
  const toolbarRefCallback = useCallback((node: HTMLDivElement | null) => {
    // Always update the shared ref so other code can read it.
    toolbarRef.current = node;
    if (!node) return;

    // ── Imperative, fully self-contained measurement engine ──────
    //
    // This closure captures nothing from React component state.  It
    // reads/writes the DOM directly.  State sync to React happens
    // through setTextHidden only at the end of a measurement, so
    // even if React is in the middle of a render, the DOM stays
    // visually consistent (we apply visibility BEFORE setting state).
    let measuring = false;
    let rafId = 0;

    const measure = () => {
      if (measuring) return;
      measuring = true;
      try {
        const els = Array.from(
          node.querySelectorAll<HTMLElement>("[data-toolbar-btn]"),
        );
        if (els.length === 0) return;

        const spans = els.map((el) => ({
          id: el.dataset.toolbarBtn as string,
          text: el.querySelector<HTMLElement>("[data-toolbar-text]"),
          chev: el.querySelector<HTMLElement>("[data-toolbar-chevron]"),
          // icon-only width (text hidden) — measured first so it's stable.
          iconWidth: el.offsetWidth,
        }));

        // Reveal all text/chevron to read true full widths.
        spans.forEach(({ text, chev }) => {
          if (text) text.style.display = "";
          if (chev) chev.style.display = "";
        });
        const fullWidths = els.map((el) => el.offsetWidth);

        const GAP = 4;
        const totalGaps = (els.length - 1) * GAP;

        // Layout constants: container padding (px-3 = 12px each side),
        // gap-2 between the left cluster and right cluster.
        // The right cluster's width is MEASURED, not counted: it is
        // a row of icon-only buttons whose own width is whatever the
        // DOM gives it, so reading `offsetWidth` covers "added a new
        // button", "button label changes", "icon-only stays icon-only
        // under all conditions" without anyone editing a number here.
        // The left cluster is the one with a foldable text-vs-icon
        // delta, so it stays in the per-button loop below.
        const PAD_X = 24;
        const FLEX_GAP = 8;
        const rightCluster = node.querySelector<HTMLElement>(
          '[data-toolbar-cluster="right"]',
        );
        // ponytail: ceiling — if the right cluster is missing (early
        // mount race), fall back to 0 so the fold still has a valid
        // available width. The next ResizeObserver / MutationObserver
        // tick re-measures. Upgrade: hoist this into a ResizeObserver
        // on the right cluster itself so it never races.
        const RIGHT_CLUSTER_W = rightCluster?.offsetWidth ?? 0;
        const available = node.offsetWidth
                        - PAD_X - FLEX_GAP - RIGHT_CLUSTER_W;

        // Progressive collapse from LEFT to RIGHT (matches the
        // documented product behavior: model hides first, then effort,
        // then ws, then sk — preserving the rightmost buttons which
        // are usually the most context-relevant).
        //
        // Algorithm:
        //   1. Start with all buttons fully visible.
        //   2. If totalFull > available, fold the leftmost button.
        //   3. Recheck; if still overflowing, fold the next leftmost.
        //   4. Stop when it fits OR every button is folded.
        //
        // Folding a button changes total width by exactly
        // (fullWidth - iconWidth) — the button stays in the row,
        // its trailing gap-1 to the next button is unchanged.

        const fold = new Array<boolean>(spans.length).fill(false);
        let current = fullWidths.reduce((a, b) => a + b, 0) + totalGaps;
        for (let i = 0; i < spans.length; i++) {
          if (current <= available) break;
          fold[i] = true;
          current -= (fullWidths[i] - spans[i].iconWidth);
        }

        // Apply visibility to DOM synchronously (user sees correct
        // layout this frame), THEN sync React state.
        spans.forEach(({ text, chev }, i) => {
          const display = fold[i] ? "none" : "";
          if (text) text.style.display = display;
          if (chev) chev.style.display = display;
        });

        setTextHidden((prev) => {
          let changed = false;
          const next = { ...prev };
          spans.forEach(({ id }, i) => {
            if (next[id] !== fold[i]) {
              next[id] = fold[i];
              changed = true;
            }
          });
          return changed ? next : prev;
        });
      } finally {
        measuring = false;
      }
    };

    const scheduleMeasure = () => {
      if (rafId !== 0) return;
      rafId = requestAnimationFrame(() => {
        rafId = 0;
        measure();
      });
    };

    // ResizeObserver: container width changes.
    const ro = new ResizeObserver(() => scheduleMeasure());
    ro.observe(node);

    // MutationObserver: button-set changes. `attributes: true` covers
    // WorkspaceSelector's text change (it updates the label when the
    // user picks a different workspace) which DOES affect width.
    const mo = new MutationObserver((mutations) => {
      const relevant = mutations.some(
        (m) =>
          m.type === "childList" ||
          m.type === "characterData" ||
          (m.type === "attributes" && m.attributeName === "data-toolbar-text"),
      );
      if (relevant) scheduleMeasure();
    });
    mo.observe(node, {
      childList: true,
      subtree: true,
      characterData: true,
      attributes: true,
      attributeFilter: ["data-toolbar-text"],
    });

    // Schedule the initial measurement for the next frame so React
    // has time to commit children (especially the conditionally-
    // rendered model/effort buttons).
    scheduleMeasure();

    // Stash teardown on the node so the next ref callback can clean
    // up.  Without this, a re-mount would leak observers.
    (node as unknown as { __toolbarTeardown__?: () => void }).__toolbarTeardown__ = () => {
      ro.disconnect();
      mo.disconnect();
      if (rafId !== 0) {
        cancelAnimationFrame(rafId);
        rafId = 0;
      }
    };
  }, []);

  // Per-agent + per-session state selectors.
  // messages and sessionStatus are split into granular selectors because
  // they change at different frequencies: messages updates every ~500ms
  // poll cycle during streaming, while sessionStatus only changes on
  // state transitions (idle→streaming→idle).  Keeping them separate
  // prevents sessionStatus-derived values (sending, etc.) from
  // re-evaluating on every poll tick.
  //
  // ADR-050 C2: `currentSessionId` is read EARLY so the live-stream
  // subscription (`useLiveStream`) can target the right session.  The
  // old code bound currentSessionId further below, after a long list
  // of toolbar refs; we keep the early read here so all subsequent
  // selectors (including the adapter live hook) see a stable session
  // id.  This is the same `useChatStore` selector that used to live
  // ~310 lines lower.
  const currentSessionId = useChatStore((s) => selectedAgentId ? s.agentStates[selectedAgentId]?.activeSessionId ?? null : null);
  /**
   * ADR-076 §决策 4: is the active session writable by the signed-in
   * account? Resolved by the shared hook so every session-scoped write
   * control in the app asks the same question the same way (see
   * `lib/session-write-access.ts` for why this is a hook, not a prop).
   *
   * Session write controls are disabled rather than hidden: a shared
   * read-only session still needs to *show* which model and workspace it
   * is using.
   */
  const readOnlySession = useActiveSessionReadOnly();
  const messages = useChatStore((s) => {
    if (!selectedAgentId) return EMPTY_MESSAGES;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return EMPTY_MESSAGES;
    return agent?.sessionStates[currentSessionId]?.messages ?? EMPTY_MESSAGES;
  });
  // ADR-050 C2: live-stream state (optimisticEntries + isThinking +
  // thinkingContent + assistantStreamingContent + isPinnedToBottom)
  // now lives in chatAdapterStore.  ChatPanel subscribes via
  // `useLiveStream(selectedAgentId, currentSessionId)` and the same
  // shape is forwarded to the v1 components (VirtualMessageList /
  // ExploreBlock) until C5 fully takes over.
  const liveState = useLiveStream(selectedAgentId, currentSessionId);
  // ADR-050 C5: optimisticEntries is no longer needed at ChatPanel level;
  // the send-message duplicate guard reads from adapterSession directly.
  const sessionStatus = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return null;
    return agent?.sessionStates[currentSessionId]?.sessionStatus ?? null;
  });
  // Remaining session fields — change infrequently, single selector is fine.
  const sessionState = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!currentSessionId) return null;
    return agent?.sessionStates[currentSessionId] ?? null;
  });
  // ADR-050 C5 (restored): the backend pushes compacting_started/ended and
  // flips isCompacting in sessionState; the chat-list bottom tail indicator
  // was removed together with the VML trailing slots in commit 574f26be.
  // It is re-added here as an in-flow tail row (like the phase indicators)
  // so it never couples to the virtualizer's row count.
  const isCompacting = sessionState?.isCompacting ?? false;
  // ADR-014: iteration-limit / loop-detected pause UX is derived directly
  // from the backend sessionStatus (Paused.detail.reason + message) — no
  // separate store flags from transient events.
  const pausedDetail = sessionStatus?.status === "paused" ? sessionStatus.detail : null;
  const iterationLimitPaused = pausedDetail?.reason === "iteration_limit"
    ? {
        iteration: pausedDetail.iteration ?? 0,
        maxIterations: pausedDetail.max_iterations ?? 0,
        message: pausedDetail.message ?? "Iteration limit reached.",
      }
    : null;
  const loopDetectedPaused = pausedDetail?.reason === "loop_detected"
    ? { message: pausedDetail.message ?? "Loop detected — session paused." }
    : null;
  const serverError = sessionState?.serverError ?? null;
  const pendingApproval = sessionState?.pendingApproval ?? {};
  const pendingQuestions = sessionState?.pendingQuestions ?? [];
  const [currentQuestionIndex, setCurrentQuestionIndex] = useState(0);
  // Reset to first question when the list changes (new question arrives)
  useEffect(() => {
    if (pendingQuestions.length === 0) {
      setCurrentQuestionIndex(0);
    } else if (currentQuestionIndex >= pendingQuestions.length) {
      setCurrentQuestionIndex(pendingQuestions.length - 1);
    }
  }, [pendingQuestions.length, currentQuestionIndex]);
  const isLoadingSession = sessionState?.isLoadingSession ?? false;
  const loadError = sessionState?.loadError ?? null;
  const todos = sessionState?.todos ?? [];
  /** Per-session queued messages — persisted in chatStore across agent switches */
  const queuedMessages = sessionState?.queuedMessages ?? [];
  // ADR-050 C5: isAssistantReplying no longer drives a trailing virtual
  // item; the v2 adapter folds the live assistant stream into blocks.
  const isThinking = liveState.isThinking;
  const thinkingStartTime = liveState.thinkingStartTime;
  const thinkingContent = liveState.thinkingContent;
  // ADR-050 C5: assistantStreamingContent / assistantStreamingStartTime
  // are no longer passed to VML — the v2 adapter folds the live assistant
  // stream into blocks (isLive: true) rendered via MessageBubble.

  // ADR-021 + ADR-049: "sending" is derived purely from sessionStatus
  // (backend source of truth). No optimistic flags — the backend pushes
  // session_state within ~50ms.
  //
  // ADR-049: instead of enumerating 4 status strings, use the pure
  // `isProcessing()` derived from `getProcessingPhase()`. The TypeScript
  // compiler will fail if any new processing phase is added without
  // updating the indicator bindings (single source of truth principle).
  const sending = isProcessing(sessionStatus);
  // ADR-049: phase is the single source of truth for indicator visibility.
  // UI banners (waiting / tool_executing / waiting_approval) are derived
  // directly from this — no flag composition. `paused` is intentionally
  // not handled here: DebugPausedBanner / RetryWaitBanner / the
  // iteration-limit & loop-detected banners (derived from Paused.detail)
  // already cover all 4 backend paths to the `Paused` state.
  const phase = getProcessingPhase(sessionStatus);
  const currentModel = sessionState?.model ?? null;
  const currentProvider = sessionState?.provider ?? null;
  const currentAccountId = sessionState?.providerAccountId ?? null;
  const currentReasoningEffort = sessionState?.reasoningEffort ?? null;

  // User profile fields — subscribed at ChatPanel level and passed as props
  // to MessageBubble so React.memo can detect profile changes (name/avatar
  // edits should update all rendered message bubbles instantly).
  const userDisplayName = useUserProfileStore((s) => s.profile.displayName);
  const userAvatarUrl = useUserProfileStore((s) => s.profile.backendAvatarUrl);
  const userBuiltinAvatarId = useUserProfileStore((s) => s.profile.backendBuiltinAvatarId);

  // Global state and actions — selectors to avoid full-store re-render
  // `effectiveConnection` is the watchdog's 6-state summary (see
  // chatStore.ts §MQTT connection watchdog). It collapses the raw
  // {known, connected, connecting, reconnecting, reason} snapshot into a
  // single value the UI can act on, and self-heals from the
  // Rust-vs-frontend inconsistency that historically left the input box
  // stuck on "正在连接 Agent..." after a wake-recovery webview reload.
  //
  // Note: the legacy `mqttConnected` boolean is still kept on the store
  // for AppLayout.tsx banner logic (see AppLayout.tsx line ~311), but
  // ChatPanel no longer reads it directly — `effectiveConnection` is the
  // single source of truth for the input box state.
  const effectiveConnection = useChatStore((s) => s.effectiveConnection);
  // Watchdog's "since" timestamp for the current non-connected state. The
  // status banner (ConnectionStatusBanner, P0-7) reads this to compute
  // the "X seconds until auto-reconnect" countdown.
  const mqttStaleSince = useChatStore((s) => s.staleSince);
  const availableModels = useChatStore((s) => s.availableModels);
  // Accounts per provider — feeds the model menu's account level.
  const providerAccounts = useChatStore((s) => s.providerAccounts);
  // Mirrored from this agent's `SessionConfig.llm_availability` retained
  // MQTT topic. Per-agent: each Runtime publishes its own value, and a
  // banner should reflect the CURRENTLY selected agent — not some other
  // agent on the same broker flashing MISSING during a reconnect race.
  // The previous boolean check caused a visible flash on every startup
  // (vault race).
  const llmAvailability = useChatStore(
    (s) =>
      selectedAgentId
        ? (s.agentStates[selectedAgentId]?.llmAvailability ?? "unspecified")
        : "unspecified",
  );
  // Stable function refs
  const {
    sendMessage,
    sendStop,
    setCurrentModel,
    setReasoningEffort,
    setAvailableModels,
    setProviderAccounts,
    continueExecution,
    resolveApproval,
    resolveApprovalByToolCallId,
    clearServerError,
  } = useChatStore.getState();
  // (currentSessionId is already declared above — ChatPanel hoists it
  // to the top of the per-session block so the live-stream subscription
  // can target the right session without a placeholder re-subscribe.)
  const currentScrollKey = selectedAgentId && currentSessionId ? `${selectedAgentId}:${currentSessionId}` : null;
  const gatewayStatus = useGatewayStore((s) => s.status);
  const { activeSkill, clearActiveSkill } = useSkillStore();

  // ── Per-session scope ──────────────────────────────────────────────
  // All mutable state that is scoped to a single session lives in this
  // hook.  On session change the entire scope is atomically reset to
  // defaults, eliminating the class of bugs where per-session refs/state
  // leak across session switches.
  const session = useSessionScope(currentSessionId);

  // ── Textarea DOM ref ────────────────────────────────────────────────
  // Needed by:
  //   - onContextMenu / paste handlers (focus check + paste-from-clipboard)
  //   - the global `desktop://file-drop` listener (only attach when the
  //     textarea — or a descendant — has focus; otherwise the drop is
  //     for a different surface, e.g. the workspace tree).
  const textareaRef = useRef<HTMLTextAreaElement | null>(null);

  // Right-click context menu for the textarea — gives the user a working
  // "Paste / Upload File" menu since Tauri WebViews do NOT render the
  // browser's native textarea context menu by default.
  const textareaMenu = useContextMenu<undefined>();

  // ── OS-level file drop listener ──────────────────────────────────────
  // Tauri v2 captures OS file drag-drop at the Rust layer
  // (`WindowEvent::DragDrop`) and re-emits the absolute paths on
  // `desktop://file-drop`. We dispatch to the same upload pipeline as
  // the paperclip button (`uploadFileAtPath`).
  //
  // Why no activeElement gating: by the time the OS drop event reaches
  // us, the user's textarea may have lost focus to the drag operation
  // itself — Tauri v2 fires `DragDrop::Drop` after the OS notifies the
  // window, which is after focus may have shifted. The chat is also the
  // only realistic drop surface (workspace panels don't have their own
  // OS-drop handler), so we accept every drop and refocus the textarea
  // so the user sees where the pending chips will land.
  //
  // Effect lifetime: set up once on mount, torn down on unmount. The
  // upload handler is read via a ref so the listener closure stays
  // stable (avoids re-subscribing on every render).
  const uploadFileAtPathRef = useRef<((filePath: string) => Promise<void>) | null>(null);
  useEffect(() => {
    // Async listener setup race guard. `listen()` resolves asynchronously;
    // React StrictMode (dev) double-invokes effects, and if the cleanup
    // runs before `listen()` resolves, `unlisten` is still `undefined` —
    // the first subscription is never torn down, leaving TWO live
    // listeners. Every drop then dispatches `uploadFileAtPath` twice,
    // surfacing as two identical attachment chips. `disposed` closes the
    // race: if cleanup ran first, the late-resolved listener cancels
    // itself immediately.
    let disposed = false;
    let unlisten: UnlistenFn | undefined;
    // Event-level dedup: a single OS drop can be re-delivered (duplicate
    // listener, WebView event duplication). The same path set arriving
    // within DEDUP_WINDOW_MS is treated as one drop.
    const DEDUP_WINDOW_MS = 500;
    let lastDropKey = "";
    let lastDropAt = 0;
    (async () => {
      try {
        const fn = await listen<string[]>("desktop://file-drop", (event) => {
          const paths = event.payload ?? [];
          if (paths.length === 0) return;
          const key = paths.join("\n");
          const now = Date.now();
          if (key === lastDropKey && now - lastDropAt < DEDUP_WINDOW_MS) {
            log.debug("[ChatPanel] duplicate desktop://file-drop event ignored", { paths });
            return;
          }
          lastDropKey = key;
          lastDropAt = now;
          // Bring the textarea to the front so the user sees where the
          // upload is going (the pending chips appear right above it).
          textareaRef.current?.focus();
          for (const p of paths) {
            void uploadFileAtPathRef.current?.(p);
          }
        });
        if (disposed) {
          // Cleanup ran while we were still subscribing — cancel now.
          fn();
          return;
        }
        unlisten = fn;
      } catch (err) {
        if (!disposed) {
          log.error("[ChatPanel] failed to subscribe to desktop://file-drop", err);
        }
      }
    })();
    return () => {
      disposed = true;
      unlisten?.();
    };
  }, []);

  // ── ADR-041 C4: ChatListAdapter ──────────────────────────────────
  // The single bridge between chatStore (data) and VirtualMessageList (render).
  // Owns: block folding, bidirectional pagination, scroll anchoring,
  // sticky-bottom state, and ensure-renderable (onLayout).
  const adapter = useChatListAdapter(selectedAgentId, currentSessionId);

  // adapterRef/sessionRef no longer needed - handleScroll and pagination
  // timer are owned by useScrollController, which has direct access to
  // adapter and containerRef.

  // Auto-collapse todo list when all tasks are completed
  useEffect(() => {
    if (todos.length === 0) return;
    if (todos.every(t => t.status === "completed")) {
      session.setTodosCollapsed(true);
    }
  }, [todos, session.setTodosCollapsed]);

  const messagesContainerRef = useRef<HTMLDivElement>(null);
  // Last known scrollTop — captured BEFORE DOM mutation each commit.
  //
  // useInsertionEffect runs in the commit phase, BEFORE the mutation step
  // (VML unmount/mount).  At this point the previous VirtualMessageList is
  // still in the DOM and container.scrollTop reflects the user’s real
  // position.  This ref is read by the snapshot-write cleanup below, which
  // runs AFTER mutation — by then the previous VML has been removed, the
  // container is empty, and the browser has clamped container.scrollTop to 0.
  //
  // Without this ref, every session switch would snapshot scrollOffset=0 and
  // the user would be restored to the TOP of every previously-visited session.
  const lastScrollTopRef = useRef(0);
  const lastScrollHeightRef = useRef(0);
  /** Content-derived blockId of the first visible block (data-driven restoration). */
  const lastFirstVisibleBlockIdRef = useRef<string | null>(null);
  /** Index of the first visible block in the adapter's blocks array (for page hint). */
  const lastFirstVisibleBlockIdxRef = useRef<number | null>(null);
  /** Adapter messageOffset at the time of the last scroll event (for snapshot). */
  const lastMessageOffsetRef = useRef<number | null>(null);
  /**
   * Viewport height captured while the container was still mounted.  The
   * snapshot writer runs in a useLayoutEffect CLEANUP, after React detached
   * the ref and the browser reports clientHeight = 0 for a detached element;
   * reading it live then would make every row look fully visible, pinning the
   * restored position to the top.  Both ref-capture sites below refresh this.
   */
  const lastClientHeightRef = useRef(0);
  useInsertionEffect(() => {
    // GUARD: only update refs when the current session has rendered blocks.
    // During a session switch the new session's adapter has NO blocks yet
    // (data not loaded).  Without this guard, useInsertionEffect (which
    // fires BEFORE useLayoutEffect cleanup) would overwrite the refs with
    // 0/null — causing the snapshot-write cleanup to save garbage
    // (scrollOffset=0, firstVisibleBlockIndex=null) instead of the old
    // session's actual position.
    //
    // For small sessions (content fits viewport, no scrollbar), onScroll
    // never fires — this effect is the ONLY ref updater.  The guard
    // `adapter.blocks.length > 0` correctly allows updates for small
    // sessions WITH data while blocking the empty-session-switch render.
    if (adapter.blocks.length > 0) {
      const container = messagesContainerRef.current;
      if (container) {
        lastScrollTopRef.current = container.scrollTop;
        lastScrollHeightRef.current = container.scrollHeight;
        lastClientHeightRef.current = container.clientHeight;
      }
      const fvbId = vmlRef.current?.getFirstVisibleBlockId();
      if (fvbId != null) lastFirstVisibleBlockIdRef.current = fvbId;
      const fvbIdx = vmlRef.current?.getFirstVisibleBlockIndex();
      if (fvbIdx != null) lastFirstVisibleBlockIdxRef.current = fvbIdx;
      lastMessageOffsetRef.current = adapter.messageOffset;
    }
  });
  /** Timestamp of the last compositionEnd event. On macOS WKWebView, compositionEnd
   *  fires BEFORE the keydown(Enter) that confirmed the IME selection, so
   *  isComposing is already false when keydown runs. We use a time-window
   *  check instead: if compositionEnd happened within the last 300ms, the
   *  Enter was almost certainly an IME confirmation, not a send intent. */
  const lastCompositionEndRef = useRef(0);
  /**
   * Imperative handle to VirtualMessageList. Exposes data-derived queries
   * about the rendered MessageBlock layout. Used here by handleScroll to pick
   * the "anchorToUser" block (the first visible block at the moment the
   * user scrolls near the top) BEFORE triggering loadMoreOlderMessages.
   * No pixel math, no estimateSize-based heuristic — a pure data lookup.
   */
  const vmlRef = useRef<VirtualMessageListHandle | null>(null);

  // ADR-009 §V-Q: server-side name only (no localStorage override).
  const agentDisplayName = selectedAgent?.display_name ?? selectedAgent?.name;

  // Read saved scroll snapshot for data-driven restoration.
  // The snapshot carries { atBottom, firstVisibleBlockId } and is consumed
  // by useScrollController to call scrollToBottom() or scrollToBlockId().
  const scrollSnapshot = currentScrollKey
    ? chatScrollSnapshots.get(currentScrollKey)
    : undefined;
  // ADR-041 C4 / ADR-050 C5: messageBlocks comes from the v2 adapter.
  // No trailing extra items (replying / compacting / working indicators)
  // - the virtualizer count === messageBlocks.length.  Live streaming
  // content is folded into blocks by the adapter (isLive: true).
  const messageBlocks = adapter.blocks;

  // ADR-081 §4.2: conversation-hit locate — scroll to + briefly highlight
  // the block containing the target raw message (pendingLocate is set by
  // chatStore.locateMessage and auto-cleared after 5s).
  const pendingLocate = sessionState?.pendingLocate ?? null;
  const [highlightBlock, setHighlightBlock] = useState<number | null>(null);
  useEffect(() => {
    if (
      !pendingLocate ||
      pendingLocate.sessionId !== currentSessionId ||
      messageBlocks.length === 0
    ) {
      setHighlightBlock(null);
      return;
    }
    // Map the raw messages[] index to the folded block that contains it.
    const targetBlock = blockIndexOfRawMessage(messageBlocks, pendingLocate.targetArrayIdx);
    if (targetBlock >= 0) {
      vmlRef.current?.scrollToIndex(targetBlock);
      setHighlightBlock(targetBlock);
      return;
    }
    // Target not in any rendered block (extreme edge — e.g. live buffer
    // before history lands): leave the highlight off, session is open.
    setHighlightBlock(null);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pendingLocate, currentSessionId, messageBlocks]);

  // ScrollController owns pagination, scroll-arrow visibility, and init-scroll
  // restoration via the data-driven snapshot { atBottom, firstVisibleBlockId }.
  const scrollController = useScrollController({
    containerRef: messagesContainerRef,
    adapter,
    vmlRef,
    sessionKey: currentScrollKey,
    initialAtBottom: scrollSnapshot?.atBottom,
    initialFirstVisibleBlockId: scrollSnapshot?.firstVisibleBlockId,
  });

  // Load available models: configured providers (from vault) + capabilities (from models API)
  const loadModels = useCallback(async () => {
    try {
      const keys = await invoke<VaultKeyEntry[]>("list_keys");

      // Accounts per provider — drives the model menu's account level.
      const accountsByProvider: Record<string, ProviderAccount[]> = {};
      for (const key of keys) {
        if (!key.account_id) continue;
        (accountsByProvider[key.provider] ??= []).push({
          accountId: key.account_id,
          alias: key.alias?.trim() || key.provider,
          preview: key.key_preview ?? "",
        });
      }
      setProviderAccounts(accountsByProvider);

      // Build (provider, configuredModelIds, modelCapabilities) tuples, skipping empty entries.
      // `models` is provider-level config shared by every account of that
      // provider, so collapse the per-account rows first — otherwise a
      // 3-key provider would refetch the same model list 3 times.
      const byProvider = new Map<string, VaultKeyEntry>();
      for (const key of keys) if (!byProvider.has(key.provider)) byProvider.set(key.provider, key);

      const entries = [...byProvider.values()].map(key => ({
        provider: key.provider,
        modelIds: key.models?.length
          ? key.models
          : key.default_model ? [key.default_model] : [],
        modelCapabilities: key.model_capabilities,
      })).filter(e => e.modelIds.length > 0);

      // Fetch capabilities for all providers in parallel
      const results = await Promise.allSettled(
        entries.map(e => fetchProviderModels(e.provider))
      );

      const allModels: ModelEntry[] = [];
      entries.forEach((entry, i) => {
        const apiModels = results[i].status === "fulfilled"
          ? (results[i].value.models ?? [])
          : [];
        for (const modelId of entry.modelIds) {
          const info = apiModels.find(m => m.id === modelId);
          allModels.push({
            name: modelId,
            provider: entry.provider,
            tool_call: info?.tool_call ?? undefined,
            reasoning: info?.reasoning ?? undefined,
            input_modalities: info?.input_modalities ?? undefined,
            default_reasoning_effort: entry.modelCapabilities?.[modelId]?.default_reasoning_effort ?? undefined,
          });
        }
      });

      // Deduplicate by model name + provider
      const uniqueModels = allModels.filter(
        (m, i, arr) => arr.findIndex(x => x.name === m.name && x.provider === m.provider) === i
      );
      setAvailableModels(uniqueModels);
    } catch {
      // Gateway may not be running
    }
  }, [setAvailableModels, setProviderAccounts]);

  useEffect(() => {
    loadModels();
  }, [gatewayStatus, loadModels]);

  // Vault keys changed elsewhere (Harness providers tab, onboarding,
  // the inline model picker below) — `models-added` is the shared
  // "keys saved" signal. Refresh the model list so newly added
  // providers/models show up immediately without a remount.
  useEffect(() => {
    const handler = () => void loadModels();
    window.addEventListener("models-added", handler);
    return () => window.removeEventListener("models-added", handler);
  }, [loadModels]);


  // Persist scroll state across top-level navigation.  AppLayout unmounts the
  // whole chat subtree for Settings/Harness/Docs/Projects, so component-local
  // refs and the virtualizer's internal offset are lost otherwise.
  //
  // deps=[currentScrollKey]: only save snapshot when session changes or when the
  // component unmounts.  Without deps the cleanup would fire on *every* re-render
  // (e.g. loadSessionMessages → isLoadingSession → re-render), overwriting the
  // unmount snapshot with a transient scrollOffset=0.
  useLayoutEffect(() => {
    // This closure is captured when the key CHANGES, so it carries the
    // OUTGOING session — the one being snapshotted when this cleanup fires.
    const sessionId = currentSessionId;
    const agentId = selectedAgentId;
    return () => {
      const key = currentScrollKey;
      if (!key) return;

      // `sending` in this closure is frozen at the moment the key changed, so
      // a stream started while the user sat in the session is invisible to it.
      // Read the status from the store by id at write time instead.
      const status =
        agentId && sessionId
          ? useChatStore.getState().agentStates[agentId]?.sessionStates[sessionId]?.sessionStatus
          : null;
      // Never snapshot a view the user never saw. If the init-scroll effect
      // never managed to position the container (remount raced the virtualizer
      // attaching its scroll element), the refs below hold the pre-position
      // scroll-top-0 reading; persisting that would pin this session to the
      // top on every later return.
      // Exception: a session left while processing always restores to the
      // bottom (atBottom is forced true below), so writing it is safe even
      // when this visit's init-scroll gave up — otherwise one give-up keeps
      // an older, possibly atBottom=false snapshot alive for every later
      // visit (a stale-restore loop that makes the bug self-reinforcing).
      //
      // ORDERING DEPENDENCY: this reads `isPositioned()` while the outgoing
      // session's flag is still set, because React runs every layout-effect
      // CLEANUP of this fiber before any of its bodies — useScrollController's
      // session-switch reset (which clears the flag) is a body and therefore
      // lands after this.  That is also why the reset must stay a plain body
      // and never be refactored into a cleanup: it would then run first and
      // silently drop the snapshot on every single session switch.
      if (!scrollController.isPositioned() && !isProcessing(status)) return;

      // Read from the guarded refs.  These hold the OLD session's
      // last-known values because:
      //   - onScroll updates them during user interaction (primary)
      //   - useInsertionEffect updates them ONLY when the container has
      //     content (guard prevents overwrite on session-switch render)
      const scrollOffset = lastScrollTopRef.current;
      const scrollHeight = lastScrollHeightRef.current;
      const firstVisibleBlockId = lastFirstVisibleBlockIdRef.current;
      // Captured while the container was still mounted — reading
      // messagesContainerRef here gives 0 (detached), which would make every
      // row look fully visible and restore at the top.
      const clientHeight = lastClientHeightRef.current;
      const distFromBottom = scrollHeight - scrollOffset - clientHeight;
      setScrollSnapshot(key, {
        atBottom: isProcessing(status) || distFromBottom <= 120,
        firstVisibleBlockId,
        messageOffset: lastMessageOffsetRef.current,
        firstVisibleBlockIndex: lastFirstVisibleBlockIdxRef.current,
      });
    };
  }, [currentScrollKey]);

  // ── Session load coordinator (mount + session switch) ────────────
  // Single effect that restores the active session whenever the selected
  // agent, its lifecycle, or the active session changes. Previously two
  // effects ("atomized mount" + "session switch") implemented this split
  // across `isInitialLoad` mutex writes; the mutex existed only to stop
  // them racing each other. Now one coordinator handles every transition
  // (mount / agent switch / session switch) exactly once, and StrictMode
  // double-runs coalesce in the single-flight store paths
  // (`openSession` / `ensureLatestInCache`).
  const prevAgentIdRef = useRef<string | null>(null);
  const prevSessionIdRef = useRef<string | null>(null);

  useEffect(() => {
    if (!selectedAgentId) return;
    const agentMeta = useAgentStore.getState().agents[selectedAgentId]?.meta;
    if (agentMeta?.lifecycle !== "sessions_ready") return; // ADR-085: capability gate

    // Read the react-tracked session id (same value the coordinator's
    // deps carry) so a session landing in chatStore re-fires the effect.
    const currentSessId = currentSessionId;
    if (!currentSessId) {
      log.debug("[ChatPanel:session-load] no active session, deferring...");
      return;
    }

    // Close session panel on agent switch (UX nicety)
    if (prevAgentIdRef.current !== null && prevAgentIdRef.current !== selectedAgentId) {
      useAgentStore.getState().reset();
    }
    // Capture the outgoing agent BEFORE overwriting the ref — the release
    // block below needs it (the session being left belongs to that agent).
    const prevAgent = prevAgentIdRef.current ?? selectedAgentId;
    prevAgentIdRef.current = selectedAgentId;

    // Release the previous session's resources (memory cleanup).
    // We do this BEFORE loading the new session so the memory is freed
    // before the new data arrives.
    //
    // Two pieces:
    //   1. `messages[]` - released unconditionally via clearSessionMessages.
    //   2. `chatAdapterStore.sessions[prevKey]` - released ONLY when the
    //      previous session is not actively streaming. If the user switched
    //      away mid-stream, the rolling `assistantStream` / `thinkingStream`
    //      ChatMessage objects are still in flight; dropping them would lose
    //      the live preview when the user switches back. Once stream ends,
    //      `record_complete` lands in `messages[]` and the next switch-away
    //      will release the (now-stale) adapter entry.
    // The session being left belongs to the PREVIOUS agent, not
    // `selectedAgentId` — on an agent switch this effect already runs with
    // the new agent id, so keying the release by `selectedAgentId` made it
    // a no-op on (newAgent, oldSessionId): the outgoing session's messages
    // and adapter blocks survived.  Coming back then hit the init-scroll
    // effect with blocksLen > 0 on the very first commit, opening the
    // handleScroll gate while the switch's scroll-event storm was still
    // in flight (the probabilistic "doesn't restore to bottom" race).
    const prevId = prevSessionIdRef.current;
    if (prevId && prevId !== currentSessId) {
      useChatStore.getState().clearSessionMessages(prevAgent, prevId);
      const prevAdapter = getChatAdapterSession(prevAgent, prevId);
      if (!prevAdapter.isAssistantReplying && !prevAdapter.isThinking) {
        releaseAdapterSession(prevAgent, prevId);
      }
    }
    prevSessionIdRef.current = currentSessId;

    const chatStore = useChatStore.getState();
    const ss0 = chatStore.agentStates[selectedAgentId]?.sessionStates[currentSessId];
    const existingMessages = ss0?.messages;
    // ADR-050 C2: optimistic overlay now lives in chatAdapterStore.
    // The mount guard's "has data?" check is widened to include the
    // adapter's optimistic entries (most often a freshly-sent user
    // message that the HTTP refresh hasn't echoed back yet).
    const adapterSession = getChatAdapterSession(selectedAgentId, currentSessId);
    const existingOptimistic = adapterSession.optimisticEntries;
    // Treat the union as "has data" — a freshly-sent optimistic user
    // message MUST NOT cause a redundant reload. The load path will
    // overwrite `messages[]` with whatever the server says; the merge
    // step inside `loadSessionMessages` will then reconcile the
    // overlay (P0-2 invariant).
    const hasMessages = !!(
      (existingMessages && existingMessages.length > 0) ||
      (existingOptimistic && existingOptimistic.length > 0)
    );

    log.debug("[ChatPanel:session-load] restore start", {
      agentId: selectedAgentId,
      sessionId: currentSessId,
      hasMessages,
    });

    // ADR-033: connectStream removed — MQTT connection is managed by Rust backend.
    // Gate the "skip reload" branch on cache integrity too: after a
    // clearSessionMessages the cache may be PARTIALLY repopulated by
    // background record_complete writes (messagesStale=true) — those
    // tail records must not mask the missing older history, so force a
    // full load until a real HTTP window has landed.
    if (!hasMessages || ss0?.messagesStale) {
      // 2a. No messages in store — load from backend (first mount or new session).
      // The scroll controller handles positioning after blocks arrive:
      // scrollToBottom() for atBottom/no-snapshot, scrollToBlockId() for browsing.
      // If the user was browsing history, load the page at their saved position
      // so the controller can restore via scrollToBlockId.  Otherwise load tail.
      const mountSnap = chatScrollSnapshots.get(`${selectedAgentId}:${currentSessId}`);
      const mountHint = (mountSnap && !mountSnap.atBottom && mountSnap.firstVisibleBlockId
        && mountSnap.messageOffset != null && mountSnap.firstVisibleBlockIndex != null)
        ? mountSnap.messageOffset + mountSnap.firstVisibleBlockIndex : null;
      const mountLoad = (mountHint != null && mountSnap)
        ? adapter.loadPageForBlockId(mountSnap.firstVisibleBlockId!, mountHint)
        : adapter.loadInitialPage();
      mountLoad
        .then(() => chatStore.loadSession(selectedAgentId, currentSessId))
        .finally(() => {
          log.debug("[ChatPanel:session-load] restore done (full)", {
            agentId: selectedAgentId,
            sessionId: currentSessId,
            messageCount: useChatStore.getState().agentStates[selectedAgentId]?.sessionStates[currentSessId]?.messages?.length ?? 0,
          });
        });
    } else {
      // 2b. Messages already in store (nav-back: same agent, same session).
      //     No reload needed — messages survive in zustand across unmount.
      chatStore.loadSession(selectedAgentId, currentSessId);
      log.debug("[ChatPanel:session-load] restore done (incremental)", {
        agentId: selectedAgentId,
        sessionId: currentSessId,
        messageCount: existingMessages.length,
      });
    }
  }, [selectedAgentId, currentSessionId, selectedAgent?.lifecycle]);

  // ── Scroll restoration ──
// Handled by ScrollController init-scroll: scrollToBottom() when atBottom
// or no snapshot, scrollToBlockId() when restoring a browsing position.

  // ── Retry session load ──────────────────────────────────────────
  // Called from VirtualMessageList when user clicks retry on load error.
  const handleRetryLoadSession = useCallback(() => {
    if (!selectedAgentId || !currentSessionId) return;
    const retrySnap = chatScrollSnapshots.get(`${selectedAgentId}:${currentSessionId}`);
    const retryHint = (retrySnap && !retrySnap.atBottom && retrySnap.firstVisibleBlockId
      && retrySnap.messageOffset != null && retrySnap.firstVisibleBlockIndex != null)
      ? retrySnap.messageOffset + retrySnap.firstVisibleBlockIndex : null;
    if (retryHint != null && retrySnap) {
      adapter.loadPageForBlockId(retrySnap.firstVisibleBlockId!, retryHint);
    } else {
      adapter.loadInitialPage();
    }
  }, [selectedAgentId, currentSessionId, adapter]);

  // ADR-041 C4: Pagination state (hasOlder, hasNewer, isLoading) and
  // actions (loadBefore, loadAfter, jumpToLatest) are now managed by the
  // adapter. ChatPanel no longer reads messageOffset/messageLimit/messageTotal
  // or defines handleNeedMore.

  // ── Scroll actions ──
  // scrollToBottom, scrollToTop, handleScroll, and the pagination timer
  // are all owned by useScrollController.  The controller's jumpToBottom
  // and jumpToTop set the state machine to 'jumping' (preventing
  // concurrent scroll operations) and delegate to adapter.jumpToLatest/
  // jumpToOldest.  The jump target effect in the controller handles the
  // actual scroll after data arrives.

  const handleSend = async () => {
    const content = session.inputValue.trim();
    const hasItems = session.pendingAttachedItems.some(
      (it) => it.status === "success" && it.item !== undefined,
    );
    const hasUploading = session.pendingAttachedItems.some(
      (it) => it.status === "uploading",
    );

    // The button stays enabled when empty; an empty click is a silent
    // no-op — no "message is empty" toast, it fired on fast-typing
    // misclicks and annoyed more than it helped.
    if (!content && !hasItems) return;
    if (sending || !selectedAgentId || hasUploading) return;

    // Collect resolved (success) AttachedItem[] for the optimistic bubble
    // and the MQTT `attached_items` payload. Backfill workspace refs
    // (`attached_file` / `attached_selection` / `attached_folder`) are
    // already AttachedItem-shaped so no post-processing is needed.
    const attachedItems = session.pendingAttachedItems
      .filter((it) => it.status === "success" && it.item !== undefined)
      .map((it) => it.item!) as AttachedItem[];

    // sendMessage is async but we fire-and-forget here.
    // The store handles all state updates internally.
    //
    // If the user is scrolled up, jump to the latest page first.  This
    // resets messageOffset to 0 so the optimistic insert in sendMessage
    // (`appendOptimisticUserMessage` in chatStore) merges into a
    // contiguous tail window when the next HTTP response lands.  We
    // delegate to scrollController.jumpToBottom (same code path as the
    // arrow button) so the state machine transitions through
    // "jumping" while the data loads, blocking the pagination timer
    // from racing with ensureLatestInCache.
    //
    // Input is cleared immediately for responsiveness; the actual send
    // waits for the jump to complete.
    session.setInputValue("");
    session.setPendingAttachedItems([]);

    if (selectedAgentId && currentSessionId) {
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }
    }

    void sendMessage(content, selectedAgentId, activeSkill?.name, attachedItems.length > 0 ? attachedItems : undefined).then(() => {
      clearActiveSkill();
    });
  };

  // Stop button dual-action:
  //   input/attachments present → queue the complete message
  //   input empty             → send queued messages + stop, or just stop
  const queueCurrentInput = () => {
    const content = session.inputValue.trim();
    const pending = session.pendingAttachedItems;
    const hasUploading = pending.some((p) => p.status === "uploading");
    const attachedItems = pending
      .filter((p) => p.status === "success" && p.item !== undefined)
      .map((p) => p.item!);
    if (
      hasUploading
      || (!content && attachedItems.length === 0)
      || !selectedAgentId
      || !currentSessionId
    ) return false;

    useChatStore.getState().addQueuedMessage(selectedAgentId, currentSessionId, {
      content,
      attachedItems,
    });
    session.setInputValue("");
    session.setPendingAttachedItems((prev) => prev.filter((p) => p.status !== "success"));
    return true;
  };

  const handleStop = async () => {
    const hasDraft = session.inputValue.trim().length > 0 || session.pendingAttachedItems.length > 0;
    if (hasDraft) {
      if (session.pendingAttachedItems.some((p) => p.status === "uploading")) return;
      if (queueCurrentInput()) return;
    }
    if (queuedMessages.length > 0 && selectedAgentId && currentSessionId) {
      // Click with queued messages: send all queued + stop current loop.
      const msgs = [...queuedMessages];
      useChatStore.getState().setQueuedMessages(selectedAgentId, currentSessionId, []);

      // Jump to bottom first if scrolled up, so optimistic inserts
      // don't break the sliding window (same as handleSend).  Use
      // scrollController.jumpToBottom so the state machine goes
      // through "jumping" while ensureLatestInCache runs.
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }

      for (const msg of msgs) {
        void sendMessage(msg.content, selectedAgentId, activeSkill?.name, msg.attachedItems).then(() => {
          clearActiveSkill();
        });
      }
      sendStop(selectedAgentId);
    } else if (selectedAgentId) {
      // No queued messages: just stop
      sendStop(selectedAgentId);
    }
  };

  // Continue button (iterationLimit / loopDetected pause): jump to
  // bottom first if scrolled up so the resumed response lands in
  // view.  Same code path as the down-arrow button — state machine
  // goes through "jumping" while ensureLatestInCache runs, blocking
  // the pagination timer from racing with the jump.
  const handleContinue = useCallback(async () => {
    if (!selectedAgentId) return;
    if (currentSessionId) {
      const ss = useChatStore.getState().getSessionState(selectedAgentId, currentSessionId);
      if (ss.messageOffset > 0) {
        await scrollController.jumpToBottom();
      }
    }
    continueExecution(selectedAgentId);
  }, [selectedAgentId, currentSessionId, continueExecution, scrollController]);

  const handleRemoveQueued = (index: number) => {
    if (selectedAgentId && currentSessionId) {
      useChatStore.getState().removeQueuedMessage(selectedAgentId, currentSessionId, index);
    }
  };

  const handleEditQueued = (index: number) => {
    const queued = queuedMessages[index];
    session.setInputValue(queued.content);
    if (queued.attachedItems.length > 0) {
      session.setPendingAttachedItems((prev) => [
        ...prev,
        ...queued.attachedItems.map((item) => ({
          tempId: `queued-${crypto.randomUUID()}`,
          status: "success" as const,
          item,
        })),
      ]);
    }
    if (selectedAgentId && currentSessionId) {
      useChatStore.getState().removeQueuedMessage(selectedAgentId, currentSessionId, index);
    }
  };

  // File upload handler: opens the dialog and dispatches to the shared
  // upload pipeline. The pipeline handles both documents and images and
  // emits a single `pendingAttachedItems` entry.
  // ADR-046: single upload entry-point for documents AND images. The
  // runtime decodes the format string and persists the blob; the desktop
  // wraps the response into a `FileUploadItem` / `ImageUploadItem`
  // metadata envelope to attach to the next user message. For images we
  // gate the upload on the current model supporting image input — if not,
  // we surface the "switch to a vision-capable model" dialog before
  // committing to a useless upload.
  const handleFileUpload = async () => {
    const { open } = await import("@tauri-apps/plugin-dialog");
    const selected = await open({
      title: "Select a file to attach",
      filters: [
        {
          name: "Common files",
          extensions: ["pdf", "docx", "pptx", "xlsx", "png", "jpg", "jpeg", "gif", "webp", "md", "txt"],
        },
        { name: "All files", extensions: ["*"] },
      ],
      multiple: false,
    });

    if (!selected) return;
    const filePath = selected as string;
    if (!filePath) return;

    // Image-only: gate on multimodal capability. Documents are unaffected —
    // they are surfaced as references regardless of model vision support.
    const filename = filePath.replace(/^.*[\\/]/, "");
    const ext = filename.split(".").pop()?.toLowerCase() ?? "";
    if (["png", "jpg", "jpeg", "gif", "webp"].includes(ext)) {
      const currentEntry = availableModels.find(
        (m) => m.name === currentModel && m.provider === currentProvider,
      );
      const supportsImage = currentEntry?.input_modalities?.includes("image");
      if (!supportsImage) {
        const imageModels = availableModels.filter((m) =>
          m.input_modalities?.includes("image"),
        );
        if (imageModels.length === 0) {
          log.warn(
            "[ChatPanel] No image-capable models available — skipping upload",
          );
          return;
        }
        session.setImageCapableModels(imageModels);
        session.setShowImageUnsupportedDialog(true);
        return;
      }
    }

    await uploadFileAtPath(filePath);
  };

  // Shared upload pipeline. The runtime accepts documents, images AND
  // plain-text / source files via the same `upload_file` command — the
  // blob store collapses unknown extensions to `.bin` on disk and the
  // `doc_reader` tool falls back to UTF-8 text extraction, so any file
  // type is a valid attachment. This wrapper only adds a single
  // `pendingAttachedItems` entry and dispatches based on file extension.
  const uploadFileAtPath = async (filePath: string) => {
    const filename = filePath.replace(/^.*[\\/]/, "");
    const ext = filename.split(".").pop()?.toLowerCase() ?? "";
    if (!filePath) return;

    // Unique per-call: `Date.now()` alone collided when N uploads fired
    // within the same ms (multi-file paste / concurrent paperclip
    // clicks). The module-scoped `pendingAttachmentSeq` counter breaks
    // ties. See declaration above.
    const tempId = `att-${Date.now()}-${++pendingAttachmentSeq}`;
    const format = ext;
    const isImage = ["png", "jpg", "jpeg", "gif", "webp"].includes(ext);

    // Pre-flight size check — the runtime enforces a 50 MiB cap
    // (see `MAX_UPLOAD_BYTES` mirror declared above + backend
    // `acowork-runtime::usecases::MAX_UPLOAD_BYTES`). Without this
    // check, a multi-hundred-MB PDF/PPTX would consume bandwidth +
    // a multipart encode roundtrip before being rejected with a
    // raw HTTP 413. Bail early with a friendly toast.
    try {
      const size = await invoke<number>("get_file_size", { filePath });
      if (size > MAX_UPLOAD_BYTES) {
        addToast({
          type: "warning",
          message: t("chatPanel.uploadTooLarge", {
            filename,
            size: formatBytes(size),
            limit: formatBytes(MAX_UPLOAD_BYTES),
          }) ?? `File "${filename}" is ${formatBytes(size)}, exceeds ${formatBytes(MAX_UPLOAD_BYTES)} limit`,
        });
        return;
      }
    } catch (err) {
      // `get_file_size` failure is non-fatal: the runtime's own
      // existence check (`Path::exists`) will produce a clean error
      // if the file is genuinely missing. We only short-circuit on
      // the size cap above; the existence + read errors flow through
      // the regular `upload_file` error path below.
      log.debug("[ChatPanel] get_file_size pre-check failed", err);
    }

    // Prerequisites — emit error chip and bail before invoking the backend.
    if (!currentSessionId) {
      session.setPendingAttachedItems(prev => [...prev, {
        tempId,
        status: "error",
        errorMessage: "No active session",
      }]);
      return;
    }
    if (!selectedAgentId) {
      session.setPendingAttachedItems(prev => [...prev, {
        tempId,
        status: "error",
        errorMessage: "No agent selected",
      }]);
      return;
    }

    // Add pending chip with uploading status. `localUrl` stays undefined for
    // documents; for images the renderer reads from the original file path
    // via the asset protocol while the upload is in flight.
    const localUrl = isImage ? convertFileSrc(filePath) : undefined;
    session.setPendingAttachedItems(prev => [...prev, {
      tempId,
      status: "uploading",
      localUrl,
    }]);

    try {
      // ADR-046: image width/height are pre-measured by the desktop via
      // `new Image()` and sent as multipart fields. Documents omit them.
      let width: number | undefined;
      let height: number | undefined;
      if (isImage && localUrl) {
        const dims = await measureImage(localUrl);
        width = dims.width;
        height = dims.height;
      }

      const result = await invoke<{
        documentId: string;
        filename: string;
        format: string;
        sizeBytes: number;
        width?: number;
        height?: number;
      }>("upload_file", {
        agentId: selectedAgentId,
        sessionId: currentSessionId,
        filePath,
        format,
        width,
        height,
      });

      const item: AttachedItem = isImage
        ? {
            type: "image_upload",
            documentId: result.documentId,
            filename: result.filename,
            format: result.format,
            sizeBytes: result.sizeBytes,
            ...(width !== undefined ? { width } : {}),
            ...(height !== undefined ? { height } : {}),
          }
        : {
            type: "file_upload",
            documentId: result.documentId,
            filename: result.filename,
            format: result.format,
            sizeBytes: result.sizeBytes,
          };

      session.setPendingAttachedItems(prev => prev.map((p) =>
        p.tempId === tempId ? { ...p, status: "success", item } : p
      ));
    } catch (err) {
      const msg = err instanceof Error ? err.message : typeof err === "string" ? err : "Upload failed";
      log.error("[ChatPanel] Attachment upload failed:", err);
      // For size-cap rejections, surface a dedicated warning toast —
      // the inline chip alone was easy to miss when the user
      // dropped several large files in a row, and the raw
      // `upload too large: ... bytes (limit 52428800)` text was
      // not user-friendly.
      if (/\bupload too large\b/i.test(msg) || /\b413\b/.test(msg)) {
        addToast({
          type: "warning",
          message: t("chatPanel.uploadTooLarge", {
            filename,
            limit: formatBytes(MAX_UPLOAD_BYTES),
          }) ?? `"${filename}" exceeds the ${formatBytes(MAX_UPLOAD_BYTES)} attachment limit`,
        });
      }
      session.setPendingAttachedItems(prev => prev.map((p) =>
        p.tempId === tempId ? { ...p, status: "error", errorMessage: msg } : p
      ));
    }
  };

  // Remove a pending attachment chip
  const handleRemovePending = (tempId: string) => {
    session.setPendingAttachedItems(prev => prev.filter((p) => p.tempId !== tempId));
  };

  // Keep the latest upload function in a ref so the long-lived
  // `desktop://file-drop` listener (set up in `useEffect` once on mount)
  // always dispatches to the current closure — without forcing the
  // listener to be torn down + re-installed on every render.
  uploadFileAtPathRef.current = uploadFileAtPath;

  // Disabled-flag for the chat input — mirrors the same condition used
  // by the JSX below. Pulled out so both the input and the right-click
  // context-menu items can read it without re-deriving.
  //
  // Uses `effectiveConnection` (the watchdog's 6-state summary) instead
  // of the raw `mqttConnected` boolean: the watchdog now flips
  // `effectiveConnection` back to "connected" within ~5s of the Rust
  // eventloop reporting connected, even if the `mqtt-status` event was
  // missed (wake-recovery webview reload, listener registration race).
  // The previous `!mqttConnected` check stayed false forever in that
  // case, locking the input box.
  //
  // Single-authority gating: the MQTT verdict ALONE decides. The old
  // `gatewayStatus !== "connected"` clause was removed — the HTTP probe
  // is display-side, and a superseded probe's late failure kept the box
  // disabled after MQTT had already healed (2026-09-22 incident).
  const inputDisabled = effectiveConnection !== "connected";

  // ── Cross-platform paste handling ──────────────────────────────────
  // Three entry points — keyboard (Ctrl+V / ⌘+V), context-menu "Paste",
  // and OS-level file drop — all funnel through `uploadFileAtPath` so
  // the UX is identical to the paperclip button.
  //
  // Why we always defer to the Rust clipboard reader instead of doing
  // frontend text-shape heuristics: complex agent messages (mermaid,
  // tables, code blocks) routinely contain lines like `/api/v1/users`,
  // `D:/data/file.txt`, or `file:///C:/...` inside markdown. Heuristics
  // that match on `s.startsWith("/")` or `^[A-Za-z]:[\\/]` mis-classify
  // those as file paths and trigger uploads against strings that are
  // not real files — producing `os error 53/161` floods and React
  // duplicate-key warnings when multiple false positives fire in the
  // same millisecond. Only the OS clipboard API can definitively say
  // "these are real files", so we treat it as the single source of
  // truth and skip the frontend heuristics entirely.

  /**
   * Insert `text` at the textarea's current caret / selection range.
   * Used by `handlePaste` and the context-menu "Paste" item — extracted
   * so the (caret-aware) insertion logic is defined exactly once.
   */
  const insertTextAtCaret = useCallback(
    (text: string) => {
      const ta = textareaRef.current;
      // `setInputValue` is a plain setter, not an updater; we read the
      // current value straight from the textarea (or the store) and
      // hand back the full next string.
      const currentValue = ta?.value ?? session.inputValue;
      if (!ta) {
        // No focused textarea — append to the end of the input value.
        session.setInputValue(currentValue + text);
        return;
      }
      const { selectionStart, selectionEnd } = ta;
      const start = selectionStart ?? currentValue.length;
      const end = selectionEnd ?? currentValue.length;
      const next = currentValue.slice(0, start) + text + currentValue.slice(end);
      session.setInputValue(next);
      const caret = start + text.length;
      requestAnimationFrame(() => {
        ta.focus();
        ta.setSelectionRange(caret, caret);
      });
    },
    [session],
  );

  /**
   * Paste handler — uploads real files from the OS clipboard; inserts
   * plain text otherwise.
   *
   * Single source of truth for file detection is the Rust command
   * `get_clipboard_file_paths`, which reads the platform's native
   * clipboard (Windows CF_HDROP, macOS NSFilenamesPboardType, Linux
   * text/uri-list) and returns the paths of files the OS confirms are
   * there. Empty result ⇒ clipboard holds no real files ⇒ fall through
   * to plain-text insertion.
   *
   * `text/plain` is consulted only for the plain-text branch (insertion).
   * It is NEVER used to detect "paths" — see block comment above.
   *
   * `preventDefault` is called whenever we replace the default paste
   * (file upload or text insertion); otherwise the browser's default
   * paste runs so the user at least sees *something* happen.
   */
  const handlePaste = useCallback(async (e: React.ClipboardEvent<HTMLTextAreaElement>) => {
    const cd = e.clipboardData;
    if (!cd) return;

    // ALWAYS preventDefault — every branch below handles insertion itself,
    // so the browser's default paste must NOT run. Leaving it enabled
    // (as the original "let default paste happen" comment suggested) is
    // exactly how the double-paste bug surfaces: in some Tauri WebView
    // paths the browser's native paste fires alongside our manual
    // `insertTextAtCaret`, producing the clipboard text twice.
    e.preventDefault();

    // Read plain text up front — needed for the "no files" branch.
    // `text/uri-list` (X11/Wayland file URIs) and `text` are also probed
    // for completeness, but in Tauri WebViews the dominant case is
    // `text/plain` carrying the actual content the user copied.
    const text =
      cd.getData("text/plain").trim() ||
      cd.getData("text").trim() ||
      cd.getData("text/uri-list").trim();

    // ALWAYS ask the Rust backend for real clipboard files. Empty result
    // means the clipboard does not currently hold any file references,
    // which is the case for both an empty clipboard AND a clipboard
    // containing arbitrary text (e.g. a copied agent message).
    let paths: string[] = [];
    try {
      paths = (await invoke<string[]>("get_clipboard_file_paths")) ?? [];
    } catch (err) {
      // Non-fatal — surface to the debug log only. The user still gets
      // their text inserted via the fallback branch below.
      log.debug("[ChatPanel] get_clipboard_file_paths failed", err);
    }
    paths = paths.filter((p): p is string => typeof p === "string" && p.length > 0);

    if (paths.length > 0) {
      // OS says these are real files — upload them. We deliberately do
      // NOT also insert the `text/plain` here: when Explorer copies N
      // files, the text payload is exactly the N paths joined by
      // newlines, and inserting them would duplicate the upload UX.
      for (const p of paths) void uploadFileAtPath(p);
      return;
    }

    // No real files on the clipboard. If the browser gave us text, insert
    // it at the caret — this is the dominant case when the user copies
    // agent output (mermaid / tables / code) and pastes it back.
    if (text) {
      insertTextAtCaret(text);
      return;
    }

    // Raster image on the clipboard (screenshot tool Ctrl+C). Two
    // surfaces to try:
    //
    //   1. `clipboardData.items` — synchronous, populated by Chromium
    //      itself when the in-WebView "Copy image" path fires. Saves
    //      an async roundtrip but is empty for OS screenshot tools
    //      (which hand the OS a `CF_BITMAP`, not a DOM-serialised
    //      image).
    //
    //   2. `navigator.clipboard.read()` — async, the only path that
    //      surfaces the OS clipboard's `CF_BITMAP` / `CF_PNG` data
    //      inside the WebView. The `paste` event is itself the
    //      user-activation gate that lets `clipboard.read()` succeed
    //      without a permission prompt.
    //
    // The `paste` event also fires before the user's first gesture
    // expires the activation, so both calls are auth-free here.
    //
    // ponytail: WebView2 has surfaced `image/png` from `clipboard.read()`
    // since Edge 98 (Feb 2022). Tauri v2 requires Edge ≥ 109 (Jan 2023),
    // so every shipping build supports this. The async fallback path
    // is unconditional — no version check — because the alternative
    // (feature-detect) costs a roundtrip on every paste.
    const imageFromDataTransfer = await extractInlineImage(cd);
    const imageBlob = imageFromDataTransfer?.blob
      ?? (await readClipboardImageBlob());
    if (imageBlob) {
      const extension = imageFromDataTransfer?.extension
        ?? extensionForMime(imageBlob.type);
      // Bridge to the existing path-based upload pipeline: hand the
      // bytes to the Rust temp-file writer, then feed the returned
      // path into the same `uploadFileAtPath` the paperclip uses.
      // The pipeline handles size cap, image dimension probing, and
      // the pending → done chip transition — see `uploadFileAtPath`.
      //
      // Transport is base64, NOT `Array.from(bytes)`: a JSON number
      // array costs ~4 text bytes + one boxed JS value per image byte,
      // so a 4K screenshot would stall the WebView main thread.
      // `readAsDataURL` is native, handles any size, and avoids the
      // `btoa(String.fromCharCode(...))` chunking trap entirely.
      try {
        const path = await invoke<string>("save_clipboard_blob", {
          dataBase64: await blobToBase64(imageBlob),
          extension,
        });
        void uploadFileAtPath(path);
        return;
      } catch (err) {
        // Failure here is rare (disk full, perms on temp dir) — fall
        // through to the silent no-op so the user can try a different
        // paste method. Surface to the debug log so the issue isn't
        // invisible.
        log.error("[ChatPanel] save_clipboard_blob failed", err);
      }
    }

    // Last-resort: some WebViews populate `cd.files` even when
    // text/plain is empty and CF_HDROP is absent. Each File has only
    // `name` (no path), so we can't upload — but inserting the
    // filename gives the user visual feedback.
    if (cd.files && cd.files.length > 0) {
      const names = Array.from(cd.files).map((f) => f.name).join("\n");
      insertTextAtCaret(names);
      return;
    }
    // else: clipboard held neither paths nor files nor text — we
    // already called preventDefault above, so nothing pastes. The
    // silent no-op is intentional (and matches the original comment
    // about "user sees *some* response" — the previous default-paste
    // fallback was the source of the double-paste bug).
  }, [insertTextAtCaret, uploadFileAtPath]);

/**
 * Try to pull a raster image out of the paste event without an
 * async `clipboard.read()` roundtrip. Modern Chromium populates
 * `clipboardData.items` with `kind=file, type=image/*` for the
 * WebView's own copy-to-clipboard path; third-party screenshot
 * tools (Windows Snipping Tool, Snipaste, ShareX) typically do
 * NOT — they hand the OS a `CF_BITMAP` and the WebView sees no
 * DOM-level image item, only the async `ClipboardItem` surface.
 *
 * Returns the blob + the file extension (without leading dot) the
 * Rust writer should use, or `null` when no inline image is
 * available and the caller should fall back to `clipboard.read()`.
 */
async function extractInlineImage(
  cd: DataTransfer,
): Promise<{ blob: Blob; extension: string } | null> {
  for (let i = 0; i < cd.items.length; i++) {
    const it = cd.items[i];
    if (it.kind !== "file") continue;
    const type = it.type;
    if (!type.startsWith("image/")) continue;
    const file = it.getAsFile();
    if (!file) continue;
    return { blob: file, extension: extensionForMime(type) };
  }
  return null;
}

/**
 * Pull a raster image `Blob` from the OS clipboard via the async
 * Clipboard API. Returns `null` if the clipboard holds no image or
 * the API is denied (the latter is rare in Tauri WebView2 but we
 * log it so the cause is visible during debugging).
 *
 * Preference order: `image/png` (lossless, native WebView2 surface
 * for the Windows Snipping Tool since 22H2), then any other raster
 * MIME the OS provides. We do NOT request `text/html` / `text/plain`
 * here — the synchronous `text` branch above already handled that.
 */
async function readClipboardImageBlob(): Promise<Blob | null> {
  if (typeof navigator === "undefined" || !navigator.clipboard?.read) {
    return null;
  }
  try {
    const items = await navigator.clipboard.read();
    // Prefer PNG so the bytes are lossless and the existing image
    // pipeline (which probes width/height via `new Image()`) gets a
    // format the runtime accepts without conversion.
    const pngItem = items.find((it) => it.types.includes("image/png"));
    const anyImageItem = items.find((it) =>
      it.types.some((t) => t.startsWith("image/")),
    );
    const picked = pngItem ?? anyImageItem;
    if (!picked) return null;
    const mime = picked.types.find((t) => t.startsWith("image/")) ?? "image/png";
    return await picked.getType(mime);
  } catch (err) {
    log.debug(
      "[ChatPanel] navigator.clipboard.read() failed (no image on clipboard, or permission denied)",
      err,
    );
    return null;
  }
}

  /**
   * Right-click handler — always opens our custom context menu because
   * Tauri WebViews do NOT render the browser's native textarea context
   * menu. Without this, the user gets no right-click UX at all.
   *
   * `preventDefault` is required to suppress the WebView's built-in
   * (mostly empty) default menu — see `useContextMenu.ts` docs.
   */
  const handleTextareaContextMenu = useCallback(
    (e: React.MouseEvent<HTMLTextAreaElement>) => {
      e.preventDefault();
      textareaMenu.openAt(e);
    },
    [textareaMenu],
  );

  // Context-menu items, memoised so menu re-renders only when the
  // disabled state changes (i.e. when there's text to paste, or not).
  //
  // MUST be declared before any early-return (e.g. the "no agents" empty
  // state below) — React's rules of hooks require hooks to be called in
  // the same order every render. An early return that skips this useMemo
  // produces "Rendered more hooks than during the previous render".
  //
  // Defined AFTER `handlePaste` and `handleFileUpload` (which it
  // references) and AFTER `inputDisabled` (declared near the upload
  // pipeline above) so all closure dependencies are in scope. Its hook
  // position — directly above the early-return — is what matters.
  const textareaMenuItems = useMemo(
    () => [
      {
        key: "paste",
        label: t("common.paste") ?? "Paste",
        icon: <Clipboard className="h-3.5 w-3.5" />,
        // ADR-076: pasting into a session we only view is session-write input.
        disabled: inputDisabled || readOnlySession,
        onClick: async () => {
          const ta = textareaRef.current;
          if (!ta) return;
          ta.focus();
          // Read whatever plain text the OS clipboard currently holds.
          // A file-only clipboard (Explorer CF_HDROP) carries NO text
          // format, so `readText()` returns "" or rejects (WebView2
          // non-secure context) — both fine: we hand the synthetic
          // event to `handlePaste`, which ALWAYS defers file detection
          // to the Rust `get_clipboard_file_paths` command and uploads
          // any real files it finds, exactly like Ctrl+V.
          let text = "";
          try {
            text = (await navigator.clipboard.readText()) ?? "";
          } catch (err) {
            log.debug(
              "[ChatPanel] clipboard.readText failed (file-only clipboard?) — deferring to native paths",
              err,
            );
          }
          const synthetic = {
            clipboardData: {
              getData: (type: string) =>
                type === "text/plain" || type === "text" || type === "text/uri-list"
                  ? text
                  : "",
              // No File objects available outside a real paste event.
              files: [] as unknown as FileList,
            },
            currentTarget: ta,
            preventDefault: () => {},
          } as unknown as React.ClipboardEvent<HTMLTextAreaElement>;
          // handlePaste is async (it invokes the Rust fallback to read
          // native clipboard file paths). Fire-and-forget is fine —
          // the upload pipeline is itself async.
          void handlePaste(synthetic);
        },
      },
      {
        key: "upload",
        label: t("common.uploadFile") ?? "Upload File",
        icon: <Upload className="h-3.5 w-3.5" />,
        disabled: inputDisabled || readOnlySession,
        onClick: () => handleFileUpload(),
      },
    ],
    [t, inputDisabled, readOnlySession, handlePaste, handleFileUpload],
  );

  // ADR-045: cancel an in-flight tool execution by tool_call_id.
  // The selected agent's active session is cancelled; we don't need toolCallId
  // here because the wire message already carries it.
  const handleToolCancel = useCallback(
    (toolCallId: string) => {
      if (!currentSessionId || !selectedAgentId) {
        console.warn('[ChatPanel] handleToolCancel: missing agent/session', {
          currentSessionId,
          selectedAgentId,
        });
        return;
      }
      useChatStore.getState().cancelTool(selectedAgentId, currentSessionId, toolCallId);
    },
    [currentSessionId, selectedAgentId],
  );

  // ADR-045: per-tool heartbeat state derived from session.
  // We subscribe to the session's toolProgress map and re-render only when
  // its identity changes (i.e. when a new tool starts OR the map gains a key).
  // The high-frequency heartbeat updates within an existing entry do NOT
  // re-render this component — only ExploreBlock re-renders for those,
  // because ExploreBlock subscribes directly via props.
  const toolProgressByToolCallId = useChatStore((s) => {
    if (!selectedAgentId || !currentSessionId) return undefined;
    return s.agentStates[selectedAgentId]?.sessionStates[currentSessionId]?.toolProgress;
  });

  // Tool approval: send decision over the authenticated HTTP API, then clear
  // inline state.
  //
  // ADR-076 §决策 4: this used to publish `approval_decision` straight to the
  // broker, where any client could have sent `{approved: true}` into someone
  // else's session — i.e. arbitrary command execution. The Runtime now
  // verifies the caller owns the session before acting.
  const handleToolApprove = async (action: "allow" | "deny", approval: ToolApprovalNeededEvent) => {
    // ADR-073: the approval event carries no `agent_id` — identity is the
    // instance key from the connection context (`selectedAgentId`).
    const instanceId = String(selectedAgentId ?? "");
    const requestId = String(approval.request_id ?? "");
    const sessionId = approval.session_id;
    try {
      if (sessionId) {
        await sessionControl.sendApproval(instanceId, sessionId, {
          request_id: requestId,
          approved: action === "allow",
          allow_all_session: false,
          reason: "",
        });
      }
    } catch (err) {
      log.error("[ChatPanel] Failed to send approval:", err);
    }
    // Clear the specific approval from the pending map by tool_call_id
    if (selectedAgentId && approval.tool_call_id) {
      resolveApprovalByToolCallId(selectedAgentId, approval.tool_call_id);
    } else {
      resolveApproval(selectedAgentId ?? "");
    }
  };

  // Ask question answer: send over the authenticated HTTP API, then clear the
  // answered question from the queue.
  // ADR-076 §决策 4: was MQTT `question_answer`.
  const handleQuestionAnswer = async (requestId: string, answer: string) => {
    if (!selectedAgentId) return;
    const instanceId = String(selectedAgentId);
    const sessionId = selectedAgentId ? useChatStore.getState().getActiveSessionId(selectedAgentId) : null;
    try {
      if (sessionId) {
        await sessionControl.sendAnswer(instanceId, sessionId, { request_id: requestId, answer });
      }
    } catch (err) {
      log.error("[ChatPanel] Failed to send question answer:", err);
    }
    // Clear the answered question from the queue by requestId
    useChatStore.getState().resolveQuestion(selectedAgentId, requestId);
  };

  // Auto-send queued messages when agent finishes execution
  useEffect(() => {
    if (!sending && queuedMessages.length > 0 && selectedAgentId && currentSessionId) {
      const msgs = [...queuedMessages];
      useChatStore.getState().setQueuedMessages(selectedAgentId, currentSessionId, []);
      for (const msg of msgs) {
        void sendMessage(msg.content, selectedAgentId, activeSkill?.name, msg.attachedItems);
      }
    }
  }, [sending]);

  // ── Empty state: no agents at all ──
  // Renders inside the same chat-panel-shaped shell as the live view
  // (CHAT_PANEL_SHELL_CN) so the surface stays opaque from first paint
  // through to session hydration — see comment on the constant above.
  if (Object.keys(useAgentStore.getState().agents).length === 0) {
    return (
      <div className={CHAT_PANEL_SHELL_CN}>
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <Bot className="mx-auto h-12 w-12 text-text-secondary " />
            <p className="mt-3 text-sm text-text-tertiary ">No agents available</p>
            <p className="mt-1 text-xs text-text-tertiary ">Connect to Gateway and install the System Agent</p>
          </div>
        </div>
      </div>
    );
  }

  // ── No agent selected ──
  if (!selectedAgent) {
    return (
      <div className={CHAT_PANEL_SHELL_CN}>
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <Bot className="mx-auto h-12 w-12 text-text-secondary " />
            <p className="mt-3 text-sm text-text-tertiary ">Select an agent to start chatting</p>
            <p className="mt-1 text-xs text-text-tertiary ">or install a new agent from the sidebar</p>
          </div>
        </div>
      </div>
    );
  }

  // ── Agent not running ──
  // Auto-sleep was retired in Sept 2026 — this branch only renders
  // when the agent has been stopped (or never started). The Start
  // button here is the user-visible recovery path.
  // Uses the chat-panel-shaped shell (see CHAT_PANEL_SHELL_CN) so the
  // surface stays opaque while stopped and the transition into the live
  // chat view is seamless — the previous flat /items-center/ div left a
  // transparent gap when the user clicked Start and the live shell
  // mounted, causing a one-frame flash.
  if (!selectedAgent.alive) {
    return (
      <div className={CHAT_PANEL_SHELL_CN}>
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <Tooltip content={t("chatPanel.startAgent")} variant="plain">
              <button
                disabled={isStarting}
                onClick={async () => {
                  // ADR-073: start is instance-scoped — use the instance key,
                  // not the package `agent_id` (ambiguous in multi-instance).
                  if (!selectedAgentId) return;
                  // tryStartAgent is the dedup gate: a second click inside the
                  // 1-3s MQTT-online window returns false silently instead of
                  // hitting the backend "already running" branch. The full
                  // orchestrator runs inside the in-flight window so the
                  // spinner stays on through session init.
                  try {
                    await useAgentStore.getState().tryStartAgent(selectedAgentId, {
                      run: (id) => startAgentAndSyncUI(id),
                    });
                  } catch (e) {
                    // Start failure must surface to the user, not leak as an
                    // unhandled rejection (2026-09-14: waitForAgentReady
                    // raced the async /start and the rejection was swallowed).
                    addToast({
                      type: "error",
                      message:
                        typeof e === "string"
                          ? e
                          : e instanceof Error
                            ? e.message
                            : String(e),
                    });
                  }
                }}
                className={cn(
                  "mx-auto flex h-20 w-20 items-center justify-center rounded-full btn-solid",
                  isStarting && "cursor-wait opacity-70",
                )}
              >
                {isStarting ? (
                  <span
                    aria-label={t("chatPanel.startAgent")}
                    className="h-8 w-8 animate-spin rounded-full border-2 border-current border-t-transparent"
                  />
                ) : (
                  <Play className="h-8 w-8" />
                )}
              </button>
            </Tooltip>
            <p className="mt-3 text-xs text-text-tertiary ">{t("chatPanel.agentStopped", { name: agentDisplayName ?? "" })}</p>
          </div>
        </div>
      </div>
    );
  }

  // ── Initializing session ──
  // Bug B v3 fix: the previous "agent not yet ready" gate (rendering
  // a "starting agent" spinner when `running && !ready`) has been
  // removed. The retired `meta.ready` flag (ADR-085: replaced by
  // `meta.lifecycle`) was pushed via MQTT retained and arrived
  // asynchronously to Runtime HTTP readiness — gating the chat view on
  // it caused the right pane to flash "starting…" for 2-3 seconds on
  // every agent switch, with no progress signal. Now:
  //   1. `selectAgent` (agentStore) gates only on `alive` — the
  //      session fetchers ride out the boot window via `with503Retry`
  //      and the Runtime's D4 gate (503 `session_not_ready` until the
  //      startup scan lands) keeps the answers honest, so no store-side
  //      lifecycle check is needed here.
  //   2. Every fetcher underneath the chat view (latest-session,
  //      loadSession, memory, workspace list, file tree, …) now
  //      routes through `with503Retry`, so a transient 503 during
  //      the boot window recovers transparently.
  //   3. The remaining "no session" gate below correctly shows a
  //      spinner only when the session is genuinely not yet open —
  //      not conflated with Runtime boot state.
  //
  // The window between MQTT pushing `running` → true and `startAgentAndSyncUI`
  // finishing its atomic initSessionForAgent chain (fetchLatestSession +
  // fetchSessions + openSession (ADR-038: was `activateSession`, now
  // `chatStore.openSession` which fires MQTT `open_session` + HTTP reload)
  // + loadSession + ensureLatestInCache).
  // During this brief window activeSessionId is still null, so without this
  // gate the chat view would mount with no session and surface the
  // "Start a conversation" placeholder for a few hundred ms — a misleading
  // "blank session" bug.  Showing a spinner here keeps the contract honest:
  // when the chat view finally mounts, the session is fully bootstrapped.
  if (!currentSessionId) {
    return (
      <div className={CHAT_PANEL_SHELL_CN}>
        <div className="flex flex-1 items-center justify-center">
          <div className="text-center">
            <div className="mx-auto h-8 w-8 animate-spin rounded-full border-2 border-zinc-300 border-t-zinc-600 dark:border-zinc-600 dark:border-t-zinc-300" />
            <p className="mt-3 text-xs text-text-tertiary ">Loading session...</p>
          </div>
        </div>
      </div>
    );
  }

  // ── Chat view ──
  // NOTE: `inputDisabled` is defined higher up (above the "no agents"
  // early-return below) so hook ordering stays stable when the agents
  // map transitions between empty and non-empty across renders. The
  // definition mirrors ADR-036 (Gateway HTTP health AND MQTT realtime
  // liveness).

  return (
    <>
      <div className={CHAT_PANEL_SHELL_CN}>
        {/* LLM config warning */}
        {llmAvailability === "missing" && (
          <div
            className="flex items-center gap-2 border-b border-amber-200 bg-amber-50 px-4 py-2 rounded-t-xl dark:border-amber-900 dark:bg-amber-950"
            role="alert"
            data-testid="llm-availability-missing"
          >
            <AlertTriangle className="h-4 w-4 text-amber-600 dark:text-amber-400" />
            <span className="text-xs text-amber-700 dark:text-amber-300">
              {t("chatPanel.llmNotConfigured")}
            </span>
          </div>
        )}
        {llmAvailability === "loading" && (
          <PlaceholderBar text={t("chatPanel.llmSyncing")} />
        )}
        {/* ADR-015: Session tab bar */}
        {selectedAgentId && <SessionTabBar agentId={selectedAgentId} />}
        {/* Messages area with drawer overlay */}
        <div
          className="relative flex flex-1 flex-col overflow-hidden"
          onWheel={(e) => {
            // Any wheel over the message area means the user is driving (or
            // about to drive) the viewport, so whatever is on screen is now a
            // reading position worth persisting — and a still-pending
            // init-scroll retry must stop fighting them for it.  See
            // ScrollController.noteViewportInteraction.
            scrollController.noteViewportInteraction();
            // The jump-to-top / jump-to-bottom buttons live as siblings of
            // the scroll container inside this `overflow-hidden` wrapper
            // (see JSX below), so the browser's scrollable-ancestor lookup
            // for a wheel event hitting a button walks up the chain,
            // finds no `overflow-y-auto` ancestor (overflow:hidden doesn't
            // count), and falls through to the body — the messages
            // container never receives the wheel.  This is invisible most
            // of the time because the buttons are tiny, but right after
            // clicking jump-to-top the cursor is still over the button
            // (and the button stays mounted for one rAF while isNearTop
            // catches up) so the user immediately wheels to scroll back
            // down and the wheel appears dead.  Forward wheel events that
            // hit a button to the scroll container; leave all other wheel
            // events alone so native scrolling inside the container works
            // exactly as before.
            if (!(e.target as HTMLElement).closest("button")) return;
            const container = messagesContainerRef.current;
            if (!container) return;
            container.scrollTop += e.deltaY;
            e.preventDefault();
          }}
        >
          {/* Monaco-style top-edge shadow — fades in when the messages
              area is scrolled away from the top (chatScrolledFromTop).
              Sits above the scroll container but below the jump-to-top
              button (same z-10, earlier in DOM). pointer-events-none so
              it never blocks selection or wheel scrolling. */}
          {chatScrolledFromTop && (
            <div
              aria-hidden="true"
              className="pointer-events-none absolute inset-x-0 top-0 z-10 h-2 bg-linear-to-b from-black/3 to-transparent dark:from-black/40"
            />
          )}
          <OverlayScrollbar
            viewportRef={messagesContainerRef}
            onViewportPointerDown={() => scrollController.noteViewportInteraction()}
            onViewportScroll={() => {
                              setChatScrolledFromTop((messagesContainerRef.current?.scrollTop ?? 0) > CHAT_TOP_SHADOW_THRESHOLD_PX);
                              lastScrollTopRef.current = (messagesContainerRef.current?.scrollTop ?? 0);
                              lastScrollHeightRef.current = (messagesContainerRef.current?.scrollHeight ?? 0);
                              lastClientHeightRef.current = (messagesContainerRef.current?.clientHeight ?? 0);
                              const fvbId = vmlRef.current?.getFirstVisibleBlockId();
                              if (fvbId != null) lastFirstVisibleBlockIdRef.current = fvbId;
                              const fvbIdx = vmlRef.current?.getFirstVisibleBlockIndex();
                              if (fvbIdx != null) lastFirstVisibleBlockIdxRef.current = fvbIdx;
                              lastMessageOffsetRef.current = adapter.messageOffset;
                              scrollController.handleScroll();
                            }}
            className="relative h-full px-4 py-3 select-text cursor-text"
            role="log"
            aria-label={t("chatPanel.ariaLabelChatMessages")}
          >
            {/* VirtualMessageList — owns useVirtualizer and handles all virtual
                scrolling rendering. Scroll behavior is owned by useScrollController.
                key={currentScrollKey} forces React to unmount/remount the entire
                component on session/agent switch.  This creates a fresh Virtualizer
                instance with scrollOffset=0, eliminating the white-screen bug where
                the old instance's scrollOffset (e.g. 5000px from a long session)
                exceeds the new session's totalSize (e.g. 800px), causing
                getVirtualItems() to return an empty array. */}
            <VirtualMessageList
              key={currentScrollKey ?? "__no_session__"}
              ref={vmlRef}
              onRetryLoadSession={handleRetryLoadSession}
              messageBlocks={messageBlocks}
              highlightBlockIndex={highlightBlock}
              sending={sending}
              pendingApproval={pendingApproval}
              currentSessionId={currentSessionId}
              selectedAgentId={selectedAgentId}
              agentDisplayName={agentDisplayName}
              selectedAgent={selectedAgent}
              userDisplayName={userDisplayName}
              userAvatarUrl={userAvatarUrl}
              userBuiltinAvatarId={userBuiltinAvatarId}
              onApprove={handleToolApprove}
              onCancelTool={handleToolCancel}
              toolProgress={toolProgressByToolCallId}
              isThinking={isThinking}
              thinkingContent={thinkingContent}
              thinkingStartTime={thinkingStartTime}
              t={t}
              adapter={adapter}
              scrollContainerRef={messagesContainerRef}
              isLoadingSession={isLoadingSession}
              loadError={loadError}
              messages={messages}
            />
            {/* ADR-050 C5: Working indicator removed.  The v2 adapter folds
                live streaming content into blocks (isLive: true); the trailing
                live block renders via ExploreBlock / StreamingSourceBlock which
                provides more information than a static "working..." label. */}
            {/* Context compaction tail indicator (regression 2026-09-06).
                Restored as an in-flow row below the last message so it does
                not couple to the VirtualMessageList row count. During
                compaction the phase indicators are suppressed so the two
                never stack. */}
            {isCompacting && (
              <div className="mt-1 ml-12 flex items-center gap-1.5" role="status">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-amber-500 animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.compacting")}
                </span>
              </div>
            )}
            {/* Debug paused banner — shown when the agent is in dev_mode and
                the debugger is currently in Stepping/Paused state. Provides
                F5 (resume) and F10 (step) actions directly from the chat.
                The banner renders its own bannerSlot wrapper and returns
                null when not visible, so no empty wrapper sits in the DOM
                when hidden — that empty wrapper would otherwise leave
                a mt-1.5 of dead space below the scroll viewport and cause
                a phantom scrollbar on empty sessions. */}
            <DebugPausedBanner />
            {/* 429 Retry wait banner — countdown + Skip Wait button, shown
                when LLM provider returns 429 with Retry-After > 10s. Same
                wrapper ownership as DebugPausedBanner. */}
            <RetryWaitBanner />
            {/* ADR-049: Phase indicators driven by getProcessingPhase().
                Each phase maps to a dot color + i18n label. paused is not
                handled here: DebugPausedBanner / RetryWaitBanner / the
                iteration-limit & loop-detected banners (derived from
                Paused.detail) cover all paths. */}
            {!isCompacting && phase === "thinking" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-purple-500 animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseThinking", { defaultValue: "Thinking…" })}
                </span>
              </div>
            )}
            {!isCompacting && phase === "waiting" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-zinc-400 dark:bg-zinc-500 animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseWaiting", { defaultValue: "Waiting for model…" })}
                </span>
              </div>
            )}
            {!isCompacting && phase === "streaming" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-[var(--color-accent)] animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseStreaming", { defaultValue: "Generating reply…" })}
                </span>
              </div>
            )}
            {!isCompacting && phase === "tool_executing" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-blue-500 animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseToolExecuting", { defaultValue: "Running tool…" })}
                </span>
              </div>
            )}
            {!isCompacting && phase === "waiting_approval" && (
              <div className="mt-1 ml-12 flex items-center gap-1.5">
                <span className="shrink-0 h-1.5 w-1.5 rounded-full bg-yellow-500 animate-pulse" />
                <span className="thinking-shimmer text-text-tertiary " style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}>
                  {t("chatPanel.phaseWaitingApproval", { defaultValue: "Waiting for tool approval…" })}
                </span>
              </div>
            )}
{/* Iteration limit pause — hint + Continue button */}
            {iterationLimitPaused && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-border-outer bg-zinc-50 px-4 py-2 text-text-secondary select-none  dark:bg-zinc-800/40 ">
                  <span
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
                  >
                    {iterationLimitPaused.message}
                  </span>
                  <button
                    onClick={handleContinue}
                    className="ml-auto flex w-fit max-w-full items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <Play className="h-3 w-3" />
                    <span>
                      Continue ({iterationLimitPaused.iteration}/{iterationLimitPaused.maxIterations})
                    </span>
                  </button>
                </div>
              </div>
            )}
            {loopDetectedPaused && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-border-outer bg-zinc-50 px-4 py-2 text-text-secondary select-none  dark:bg-zinc-800/40 ">
                  <span
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}
                  >
                    {loopDetectedPaused.message}
                  </span>
                  <button
                    onClick={handleContinue}
                    className="ml-auto flex w-fit max-w-full items-center gap-1 rounded bg-[var(--color-accent)] px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <Play className="h-3 w-3" />
                    <span>Continue</span>
                  </button>
                </div>
              </div>
            )}
            {/* Ask question cards — shown when LLM asks the user questions */}
            {serverError && (
              <div className="mt-1.5 flex justify-center px-6">
                <div className="inline-flex flex-wrap items-center gap-x-2 gap-y-2 rounded-md border border-amber-500/30 bg-amber-500/10 px-4 py-2 text-amber-600 select-none dark:border-amber-500/40 dark:bg-amber-500/15 dark:text-amber-400">
                  <AlertTriangle className="h-4 w-4 shrink-0" />
                  <div className="min-w-0 flex-1">
                    <div className="whitespace-pre-wrap break-words" style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.85)" }}>
                      {/* ADR-061 §19.4: ContextOverflow carries a localized
                          hint instead of the raw backend capability message */}
                      {serverError.errorType === "ContextOverflow"
                        ? t("chatError.contextOverflow")
                        : serverError.content}
                    </div>
                    {serverError.errorDetail && (
                      <details className="mt-1">
                        <summary className="cursor-pointer text-xs text-amber-500/70 hover:text-amber-500 select-none">
                          Details
                        </summary>
                        <pre className="mt-1 max-h-40 overflow-auto rounded bg-black/5 dark:bg-white/5 p-2 text-xs text-amber-500/70 whitespace-pre-wrap break-all">
                          {serverError.errorDetail}
                        </pre>
                      </details>
                    )}
                  </div>
                  <button
                    onClick={() => {
                      if (selectedAgentId && currentSessionId) {
                        clearServerError(selectedAgentId, currentSessionId);
                      }
                    }}
                    className="ml-auto flex w-fit items-center gap-1 rounded bg-amber-500 px-2 py-0.5 text-11 font-medium text-white transition-colors hover:brightness-90"
                    style={{ fontSize: "calc(var(--ui-font-size, 0.875rem) * 0.9)" }}
                  >
                    <X className="h-3 w-3" />
                    <span>Dismiss</span>
                  </button>
                </div>
              </div>
            )}
            {pendingQuestions.length > 0 && (
              <div className="space-y-1">
                {/* Progress indicator */}
                <div className="flex items-center gap-2 px-1">
                  <div className="flex items-center gap-1.5 text-xs text-text-tertiary ">
                    <span className="font-medium text-text-secondary ">
                      {currentQuestionIndex + 1} / {pendingQuestions.length}
                    </span>
                    <span>{t("askQuestionCard.questions")}</span>
                  </div>
                  {pendingQuestions.length > 1 && (
                    <div className="flex items-center gap-1 ml-auto">
                      <button
                        onClick={() => setCurrentQuestionIndex((i) => Math.max(0, i - 1))}
                        disabled={currentQuestionIndex === 0}
                        className="rounded p-0.5 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300 disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        aria-label={t("askQuestionCard.previous")}
                      >
                        <ChevronLeft className="h-3.5 w-3.5" />
                      </button>
                      {/* Dots */}
                      <div className="flex items-center gap-1 mx-1">
                        {pendingQuestions.map((_, idx) => (
                          <button
                            key={idx}
                            onClick={() => setCurrentQuestionIndex(idx)}
                            className={`h-1.5 w-1.5 rounded-full transition-all ${
                              idx === currentQuestionIndex
                                ? "bg-zinc-500 dark:bg-zinc-300 w-3"
                                : "bg-zinc-300 dark:bg-zinc-600 hover:bg-zinc-400 dark:hover:bg-zinc-500"
                            }`}
                            aria-label={`${t("askQuestionCard.goTo")} ${idx + 1}`}
                          />
                        ))}
                      </div>
                      <button
                        onClick={() => setCurrentQuestionIndex((i) => Math.min(pendingQuestions.length - 1, i + 1))}
                        disabled={currentQuestionIndex === pendingQuestions.length - 1}
                        className="rounded p-0.5 text-text-tertiary hover:text-zinc-600 dark:hover:text-zinc-300 disabled:opacity-30 disabled:cursor-not-allowed transition-colors"
                        aria-label={t("askQuestionCard.next")}
                      >
                        <ChevronRight className="h-3.5 w-3.5" />
                      </button>
                    </div>
                  )}
                </div>
                {/* Current question card */}
                <AskQuestionCard
                  key={pendingQuestions[currentQuestionIndex].request_id}
                  event={pendingQuestions[currentQuestionIndex]}
                  agentId={selectedAgentId ?? ""}
                  sessionId={currentSessionId}
                  onAnswer={handleQuestionAnswer}
                />
              </div>
            )}
          </OverlayScrollbar>
          {/* Scroll-to-top button - visible when scrolled down > 1 screen.
              Default opacity is reduced so the button does not visually crowd
              the chat content; it goes fully opaque on hover/focus for clear
              affordance. The existing `transition-all` covers the opacity
              animation, and the `hover:bg-zinc-200` background change is
              independent of the opacity change so both compose cleanly. */}
          {scrollController.showScrollToTop && (
            <button
              onClick={scrollController.jumpToTop}
              className="absolute top-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
              aria-label="Scroll to top"
            >
              <ChevronsUp className="h-4 w-4 text-text-tertiary " />
            </button>
          )}
          {/* Scroll-to-bottom button — visible when scrolled up > 1 screen.
              Same default-reduced / hover-full-opacity treatment as the
              scroll-to-top button for visual consistency. */}
          {scrollController.showScrollToBottom && (
            <button
              onClick={scrollController.jumpToBottom}
              className="absolute bottom-3 right-4 z-10 rounded-full bg-zinc-100 dark:bg-zinc-700 border border-zinc-200 dark:border-zinc-600 shadow-md p-1.5 opacity-40 hover:opacity-100 focus-visible:opacity-100 hover:bg-zinc-200 dark:hover:bg-zinc-600 transition-all animate-in fade-in zoom-in"
              aria-label={t("chatPanel.ariaLabelScrollToBottom")}
            >
              <ChevronsDown className="h-4 w-4 text-text-tertiary " />
            </button>
          )}
        </div>

        {/* Todo list box — above the message queue, same collapsible style.
          Shows current task list from todo_write tool calls. */}
        {todos.length > 0 && (
          <div className="mx-12 mb-0 rounded-t-md border border-b-0 border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-800/60 overflow-hidden">
            <button
              className="flex items-center w-full px-2.5 py-1.5 border-b border-border-divider hover:bg-zinc-100 dark:hover:bg-zinc-700/30 transition-colors"
              onClick={() => session.setTodosCollapsed(!session.todosCollapsed)}
            >
              {session.todosCollapsed ? (
                <ChevronRight className="h-3 w-3 mr-1 text-text-tertiary  shrink-0" />
              ) : (
                <ChevronDown className="h-3 w-3 mr-1 text-text-tertiary  shrink-0" />
              )}
              <span className="min-w-0 truncate text-10 font-medium text-text-tertiary  uppercase tracking-wider">
                {(() => {
                  const completed = todos.filter(t => t.status === "completed").length;
                  const total = todos.length;
                  const currentTodo = todos.find(t => t.status === "in_progress");
                  const isAllCompleted = completed === total && total > 0;
                  return (
                    <>
                      <span className="inline whitespace-nowrap">{t("chatPanel.taskList", { completed, total })}</span>
                      {!isAllCompleted && currentTodo && (
                        <>
                          <span className="inline-block w-8"/>
                          <span className="normal-case text-text-tertiary  truncate">
                            {t("chatPanel.currentTask", { current: currentTodo.content })}
                          </span>
                        </>
                      )}
                    </>
                  );
                })()}
              </span>
            </button>
            {!session.todosCollapsed && (
              <div className="max-h-[7.5rem] overflow-y-auto">
                {todos.map((item) => {
                  const isCompleted = item.status === "completed";
                  const isInProgress = item.status === "in_progress";
                  return (
                    <div
                      key={item.id}
                      className={`flex items-start gap-1.5 px-2.5 py-1.5 border-b border-border-divider last:border-b-0 ${listRowHover}`}
                    >
                      <span className={cn(
                        "shrink-0 mt-0.5 select-none",
                        isCompleted
                          ? "text-text-tertiary "
                          : isInProgress
                            ? "text-text-tertiary "
                            : "text-text-tertiary "
                      )}>
                        {isCompleted ? (
                          <CircleDot className="h-3.5 w-3.5" strokeWidth={2.25} />
                        ) : isInProgress ? (
                          <Loader className="h-3.5 w-3.5 animate-spin" strokeWidth={2.25} />
                        ) : (
                          <Circle className="h-3.5 w-3.5" strokeWidth={2.25} />
                        )}
                      </span>
                      <span className={cn(
                        "flex-1 min-w-0 text-xs leading-relaxed truncate",
                        isCompleted
                          ? "text-text-tertiary  line-through"
                          : isInProgress
                            ? "text-text-secondary  font-medium"
                            : "text-text-secondary "
                      )}>
                        {item.content}
                      </span>
                    </div>
                  );
                })}
              </div>
            )}
          </div>
        )}

        {/* Queued messages box — separate box above the input area,
          flush against input, slightly narrower for layered depth */}
        {queuedMessages.length > 0 && (
          <div className={cn(
            "mx-12 mb-0 border border-b-0 border-zinc-200 dark:border-zinc-800 bg-zinc-50/80 dark:bg-zinc-800/60 overflow-hidden",
            todos.length > 0 ? "" : "rounded-t-md"
          )}>
            <div className="flex items-center px-2.5 py-1.5 border-b border-border-divider">
              <span className="text-10 font-medium text-text-tertiary  uppercase tracking-wider">
                {t("chatPanel.messageQueue", { count: queuedMessages.length })}
              </span>
            </div>
            <div className="max-h-[7.5rem] overflow-y-auto">
              {queuedMessages.map((msg, i) => (
                <div
                  key={i}
                  className={`group flex items-start gap-1.5 px-2.5 py-1.5 border-b border-border-divider last:border-b-0 ${listRowHover}`}
                >
                  <span className="shrink-0 text-10 mt-0.5 text-text-tertiary  select-none">{i + 1}.</span>
                  <span className="flex-1 min-w-0 text-xs text-text-secondary truncate leading-relaxed">
                    {msg.content || msg.attachedItems.map((item) => item.type === "file_upload" || item.type === "image_upload" ? item.filename : item.name).join(", ")}
                  </span>
                  <div className="flex items-center gap-0.5 shrink-0 opacity-0 group-hover:opacity-100 transition-opacity">
                    <button
                      type="button"
                      onClick={() => handleEditQueued(i)}
                      className="rounded-sm p-0.5 text-text-tertiary hover:text-[var(--color-accent)] hover:bg-[var(--color-accent)]/10"
                      aria-label={`Edit message ${i + 1}`}
                    >
                      <Pencil size={12} />
                    </button>
                    <button
                      type="button"
                      onClick={() => handleRemoveQueued(i)}
                      className="rounded-sm p-0.5 text-text-tertiary hover:text-red-500 hover:bg-red-50 dark:hover:text-red-400 dark:hover:bg-red-900/30"
                      aria-label={`Remove message ${i + 1}`}
                    >
                      <X size={12} />
                    </button>
                  </div>
                </div>
              ))}
            </div>
          </div>
        )}

        {/* Unified input container with toolbar */}
        <div className="mx-6 mb-3 rounded-xl border border-chat-input-border bg-chat-input-bg">
          {/* Active skill badge */}
          {activeSkill && (
            <div className="flex items-center gap-1 px-3 pt-2">
              <span className="inline-flex items-center gap-1 rounded bg-[var(--color-accent)]/10 px-1.5 py-0.5 text-xs font-medium border border-[var(--color-accent)]/20" style={{ color: "var(--color-accent)" }}>
                /{activeSkill.name}
                <button
                  type="button"
                  onClick={clearActiveSkill}
                  className="ml-0.5 inline-flex items-center justify-center rounded-sm hover:bg-[var(--color-accent)]/15"
                  aria-label={t("chatPanel.ariaLabelClearActiveSkill")}
                >
                  <X size={12} />
                </button>
              </span>
            </div>
          )}

          {/* Pending attachment chips (ADR-046: unified entry-point).
              Documents use the existing `DocumentChip`; images get a thumbnail
              variant. Both render from `pendingAttachedItems`. */}
          {session.pendingAttachedItems.length > 0 && (
            <div className="flex flex-wrap items-center gap-1.5 px-3 pt-2">
              {session.pendingAttachedItems.map((p) => {
                const item = p.item;
                // Workspace reference: already-resolved `attached_*` items.
                if (item && (item.type === "attached_file"
                  || item.type === "attached_selection"
                  || item.type === "attached_folder")) {
                  return (
                    <DocumentChip
                      key={p.tempId}
                      filename={item.name}
                      format="workspace"
                      status={p.status}
                      errorMessage={p.errorMessage}
                      onRemove={() => handleRemovePending(p.tempId)}
                    />
                  );
                }
                // Image upload (or pre-upload with only localUrl set).
                if (item && item.type === "image_upload") {
                  return (
                    <div
                      key={p.tempId}
                      className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700"
                    >
                      <img
                        src={p.localUrl ?? `/api/agents/${selectedAgentId ?? ""}/files/${item.documentId}`}
                        alt={item.filename}
                        className="h-full w-full object-cover"
                      />
                      <button
                        type="button"
                        onClick={() => handleRemovePending(p.tempId)}
                        className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
                        aria-label={`Remove ${item.filename}`}
                      >
                        <X size={10} />
                      </button>
                    </div>
                  );
                }
                // Pre-upload preview for an image that has only a localUrl.
                if (!item && p.localUrl) {
                  return (
                    <div
                      key={p.tempId}
                      className="group relative h-14 w-14 shrink-0 overflow-hidden rounded-md border border-zinc-200 dark:border-zinc-700"
                    >
                      <img
                        src={p.localUrl}
                        alt=""
                        className="h-full w-full object-cover opacity-60"
                      />
                      <button
                        type="button"
                        onClick={() => handleRemovePending(p.tempId)}
                        className="absolute -right-0.5 -top-0.5 flex h-4 w-4 items-center justify-center rounded-full bg-red-500 text-white opacity-0 transition-opacity group-hover:opacity-100"
                        aria-label="Remove attachment"
                      >
                        <X size={10} />
                      </button>
                    </div>
                  );
                }
                // Document upload (or upload-in-flight without resolved item).
                const filename = item && item.type === "file_upload" ? item.filename : "";
                const format = item && item.type === "file_upload" ? item.format : "";
                const size = item && item.type === "file_upload" ? item.sizeBytes : undefined;
                return (
                  <DocumentChip
                    key={p.tempId}
                    filename={filename}
                    format={format}
                    size={size}
                    status={p.status}
                    errorMessage={p.errorMessage}
                    onRemove={() => handleRemovePending(p.tempId)}
                  />
                );
              })}
            </div>
          )}
          {/* Attached context chips (from right-click "Add to Chat") */}
          <AttachedContextChips />
          {/* P0-7: live connection-status banner with auto-reconnect countdown.
              Renders nothing when the connection is healthy — only shows when
              the watchdog is actively trying to recover (connecting / stale /
              disconnected). The placeholder below is the fallback for the
              rare case where the banner isn't mounted (e.g. while the
              watchdog is still in its initial idle state). */}
          <ConnectionStatusBanner
            effectiveConnection={effectiveConnection}
            staleSince={mqttStaleSince}
          />
          {/* Textarea area — borderless, transparent background */}
          <textarea
            ref={textareaRef}
            value={session.inputValue}
            onChange={(e) => session.setInputValue(e.target.value)}
            placeholder={readOnlySession
              ? t("chatPanel.readOnlySessionPlaceholder")
              : t(
                  `chatPanel.${getInputPlaceholderKey(gatewayStatus, effectiveConnection, !!activeSkill, sending)}`,
                )}
            disabled={inputDisabled || readOnlySession}
            className="w-full resize-none border-0 bg-transparent p-3 pb-2 outline-none placeholder:text-text-disabled  disabled:cursor-not-allowed disabled:opacity-50 max-h-48 overflow-y-auto min-h-[5rem]"
            style={{ fontSize: "var(--ui-font-size, 0.875rem)" }}
            onKeyDown={(e) => {
              if (e.key !== "Enter" || e.shiftKey) return;
              // On macOS, compositionEnd fires before keydown(Enter) when the user
              // presses Enter to confirm an IME selection. isComposing is already
              // false by then, so we check a time window instead.
              const elapsed = Date.now() - lastCompositionEndRef.current;
              if (elapsed < 300) return; // IME confirmation — do not send
              e.preventDefault();
              // Enter is ALWAYS a send action — never a stop.
              // To stop, the user MUST click the Stop button (handleStop).
              // Rationale: spurious Enter events (macOS keyboard repeat, WKWebView
              // focus/keyboard quirks during window resize, accidental keypresses)
              // previously triggered urgent_stop and silently killed the stream.
              if (!sending) {
                handleSend();
                return;
              }
              queueCurrentInput();
            }}
            onCompositionEnd={() => {
              lastCompositionEndRef.current = Date.now();
            }}
            onPaste={handlePaste}
            onContextMenu={handleTextareaContextMenu}
          />
          {/* Custom right-click menu — Tauri WebViews don't render the
              browser's native textarea menu, so we mount our own. */}
          <ContextMenu
            isOpen={textareaMenu.isOpen}
            menuProps={textareaMenu.menuProps}
            items={textareaMenuItems}
            payload={textareaMenu.payload}
            selectionAtOpen={textareaMenu.selectionAtOpen}
            onClose={textareaMenu.close}
          />

          {/* Bottom toolbar — @container for responsive button text collapse */}
          <div
            ref={(node) => {
              // Cleanup the previous install (if any) before handing
              // off to the ref callback, which re-installs on the new
              // node.  This handles re-mounts cleanly.
              const prev = (toolbarRef.current as unknown as { __toolbarTeardown__?: () => void } | null);
              prev?.__toolbarTeardown__?.();
              toolbarRefCallback(node);
            }}
            className="@container/tb flex items-center justify-between gap-2 px-3 pb-2 min-w-[264px]"
          >
            {/* Left: feature buttons */}
            <div className="flex items-center gap-1 min-w-0 overflow-visible">
             {/* Model switcher — shown for any configured model(s); the
                  button doubles as the current-model indicator (ADR:
                  single-model setups still need to SEE which model is in
                  use), plus the "Add Models" entry point. Only enabled
                  when agent is running. */}
              {availableModels.length > 0 && selectedAgent?.alive && (
                <ModelMenu
                  wrapperRef={modelBtnRef}
                  textHidden={textHidden.model}
                  models={availableModels}
                  accountsByProvider={providerAccounts}
                  currentModel={currentModel}
                  currentProvider={currentProvider}
                  currentAccountId={currentAccountId}
                  onSelect={(m, p, a) => selectedAgentId && setCurrentModel(m, p, selectedAgentId, a)}
                  btnId="model"
                  readOnly={readOnlySession}
                />
              )}
              {/* Reasoning effort toggle — shown when session has a non-null reasoningEffort (null = provider doesn't support reasoning) */}
              {selectedAgent?.alive && currentReasoningEffort != null && (
                <ReasoningEffortMenu
                  wrapperRef={effortBtnRef}
                  textHidden={textHidden.effort}
                  effort={currentReasoningEffort}
                  onChange={(e) => selectedAgentId && setReasoningEffort(e, selectedAgentId)}
                  btnId="effort"
                  readOnly={readOnlySession}
                />
              )}
              {/* Workspace button */}
              <div ref={wsBtnRef} className="min-w-0">
                <WorkspaceSelector textHidden={textHidden.ws} readOnly={readOnlySession} />
              </div>
              {/* Skills dropdown */}
              <div ref={skBtnRef} className="min-w-0">
                <SkillsPanel textHidden={textHidden.sk} readOnly={readOnlySession} />
              </div>
            </div>

            {/* Right: session visibility + usage + attachment + send.
                `data-toolbar-cluster="right"` marks the cluster so the
                collapse algorithm can MEASURE its offsetWidth (instead
                of counting buttons × 28 + gaps × 4 by hand). Adding a
                new icon-only button to this cluster changes the
                measured width on the next frame; the fold math picks
                it up with no code change. The left cluster keeps the
                per-button fold because its text-vs-icon delta is what
                progressive collapse is for. */}
            <div data-toolbar-cluster="right" className="flex shrink-0 items-center gap-1">
              {/* Context usage icon — shown when session is active */}
              {selectedAgentId && currentSessionId && <ContextUsageIcon agentId={selectedAgentId} sessionId={currentSessionId} />}

              {/* ADR-076 §决策 4: per-session read visibility (owner only).
                  Icon-only, so it rides with the other icon-only right-cluster
                  controls and stays out of the collapsible text row. */}
              {selectedAgentId && currentSessionId && (
                <SessionVisibilityToggle agentId={selectedAgentId} sessionId={currentSessionId} />
              )}

              {/* File upload button — single entry point for documents and images.
                  Placed adjacent to send so attachments sit with the action that
                  consumes them. ADR-076: an attachment is session-write input,
                  so it is disabled on a read-only session. */}
              <Tooltip content={readOnlySession ? t("chatPanel.readOnlySession") : t("chatPanel.uploadHint")}>
                <button
                  className={toolbarIconButton}
                  onClick={handleFileUpload}
                  disabled={!currentSessionId || !selectedAgentId || readOnlySession}
                  aria-label={t("chatPanel.uploadFile")}
                >
                  <Paperclip size={14} />
                </button>
              </Tooltip>

              {/* Send/Stop button with tooltip above */}
              <Tooltip content={sending
                ? (session.inputValue.trim()
                  ? t("chatPanel.addToQueue")
                  : queuedMessages.length > 0
                    ? t("chatPanel.sendQueuedAndStop")
                    : t("chatPanel.stop"))
                : t("chatPanel.sendMessage")}>
                <button
                  // The `sending` branch needs its own `disabled:` styles too:
                  // a viewer watching the owner's stream lands here with the
                  // button disabled, and an unstyled disabled button reads as
                  // a live stop control.
                  className={cn(
                    toolbarIconButton,
                    sending && "text-[var(--color-accent)] hover:bg-[var(--color-accent)]/10",
                  )}
                  onClick={sending ? handleStop : handleSend}
                  disabled={
                    // ADR-076 §决策 4: a session shared with us is read-only
                    // — sending is write-gated server-side (404), and so is
                    // stopping someone else's stream.
                    //
                    // Empty input is NOT a disable condition: a dead button
                    // has no affordance to explain why, so the button stays
                    // live; an empty click in `handleSend` is a silent
                    // no-op. Uploading attachments still disables (the
                    // payload can't be assembled mid-upload).
                    readOnlySession
                      ? true
                      : sending
                        ? false
                        : (inputDisabled
                          || session.pendingAttachedItems.some((p) => p.status === "uploading"))
                  }
                  aria-label={sending ? (session.inputValue.trim() ? t("chatPanel.addToQueue") : queuedMessages.length > 0 ? t("chatPanel.sendQueuedAndStop") : t("chatPanel.stop")) : t("chatPanel.sendMessage")}
                >
                  {sending ? <Square size={16} fill="currentColor" /> : <Send size={16} />}
                </button>
              </Tooltip>
            </div>
          </div>
        </div>
      </div>

      {/* Image unsupported dialog */}
      <UnsupportedImageDialog
        open={session.showImageUnsupportedDialog}
        models={session.imageCapableModels}
        onSelect={(model: string, provider: string) => {
          if (selectedAgentId) {
            setCurrentModel(model, provider, selectedAgentId);
            session.setShowImageUnsupportedDialog(false);
          }
        }}
        onClose={() => session.setShowImageUnsupportedDialog(false)}
      />
    </>
  );
}

/** Shell tools (bash, powershell, shell) need Terminal icon and command preview. */

/** Dialog shown when user tries to upload an image but the current model doesn't support it */
function UnsupportedImageDialog({
  open,
  models,
  onSelect,
  onClose,
}: {
  open: boolean;
  models: ModelEntry[];
  onSelect: (model: string, provider: string) => void;
  onClose: () => void;
}) {
  const { t } = useTranslation();
  if (!open) return null;

  return (
    <div className="fixed inset-0 z-[60] flex items-center justify-center bg-modal-overlay" onClick={onClose}>
      <div
        className="w-[400px] overflow-hidden rounded-md border border-border-outer bg-modal-surface shadow-xl flex flex-col"
        onClick={(e) => e.stopPropagation()}
      >
        <h3 className="shrink-0 px-6 pt-6 pb-2 text-sm font-semibold text-text ">
          {t("chatPanel.imageUnsupportedTitle")}
        </h3>
        <p className="px-6 pb-4 text-xs text-text-tertiary ">
          {t("chatPanel.imageUnsupportedDesc")}
        </p>

        <div className="max-h-[240px] overflow-y-auto px-6 pb-2">
          {models.map((m) => (
            <button
              key={`${m.name}::${m.provider}`}
              type="button"
              onClick={() => {
                onSelect(m.name, m.provider);
              }}
              className={`flex w-full items-center justify-between px-2.5 py-1.5 text-xs transition-colors rounded-md text-text-secondary  ${listRowHover}`}
            >
              <span className="flex items-center gap-1.5 min-w-0">
                <Image size={12} className="shrink-0 text-blue-400" />
                <span className="font-medium truncate">
                  {(() => {
                    if (!m.name.includes('/')) return m.name;
                    const parts = m.name.split('/');
                    const prefix = parts[0];
                    const modelName = parts.slice(1).join('/');
                    return modelName.length > prefix.length ? modelName : m.name;
                  })()}
                </span>
                <span className="flex items-center gap-0.5 shrink-0">
                  {m.tool_call && <Wrench size={10} className="text-text-tertiary" />}
                  {m.reasoning && <Brain size={10} className="text-purple-400" />}
                </span>
              </span>
              <span className="text-10 text-text-tertiary  shrink-0 ml-2">
                {m.provider}
              </span>
            </button>
          ))}
        </div>

        <div className="shrink-0 flex items-center justify-end gap-2 border-t border-border-divider px-6 py-4">
          <button
            onClick={onClose}
            className="rounded-md px-4 py-1.5 text-xs font-medium text-text-secondary hover:bg-zinc-100  dark:hover:bg-zinc-700"
          >
            {t("chatPanel.close")}
          </button>
        </div>
      </div>
    </div>
  );
}

/**
 * Group flat `models` into `[provider, model[]]` pairs sorted
 * alphabetically (case-insensitive) by provider, then by model name.
 *
 * Stable ordering matters because the menu has a hover-fly-out anchored
 * to a specific row's viewport rect; if providers reshuffle on every
 * vault reload, the fly-out's cached position desyncs from the row the
 * user is actually hovering (deepseek used to jump from first to last).
 *
 * `query` filters both `model.name` and `model.provider` (case-insensitive
 * substring) — empty string disables the filter.
 *
 * Exported for unit testing; the ModelMenu's own useMemo is the only
 * call site.
 */
export function groupModelsByProvider<
  T extends { name: string; provider: string },
>(
  models: T[],
  query: string,
): Array<readonly [string, T[]]> {
  const q = query.trim().toLowerCase();
  const map = new Map<string, T[]>();
  for (const m of models) {
    if (q && !m.name.toLowerCase().includes(q) && !m.provider.toLowerCase().includes(q)) continue;
    const bucket = map.get(m.provider);
    if (bucket) bucket.push(m);
    else map.set(m.provider, [m]);
  }
  const cmp = (a: string, b: string) => a.toLowerCase().localeCompare(b.toLowerCase());
  return Array.from(map.entries())
    .map(([provider, ms]) => [provider, [...ms].sort((a, b) => cmp(a.name, b.name))] as const)
    .sort(([a], [b]) => cmp(a, b));
}

/**
 * Model selector popup. Models are grouped by provider (sticky header);
 * when a provider holds more than one API key, picking a model drills into
 * an account level (`provider → model → account`) instead of silently using
 * the first key. Single-key providers keep the flat two-level behaviour.
 */
function ModelMenu({
  models,
  accountsByProvider,
  currentModel,
  currentProvider,
  currentAccountId,
  onSelect,
  textHidden,
  wrapperRef: externalRef,
  btnId,
  readOnly,
}: {
  models: { name: string; provider: string; tool_call?: boolean; reasoning?: boolean; input_modalities?: string[] }[];
  /** Accounts per provider id; provider missing or single-entry = no account level */
  accountsByProvider: Record<string, ProviderAccount[]>;
  currentModel: string | null;
  currentProvider: string | null;
  currentAccountId: string | null;
  onSelect: (model: string, provider: string, accountId?: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
  /** Toolbar button id used by ChatPanel's collapse observer */
  btnId?: string;
  /** ADR-076 §决策 4: session is not writable by the signed-in account. */
  readOnly?: boolean;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const [showAddDialog, setShowAddDialog] = useState(false);
  /** Set while drilling into a multi-key provider's accounts. */
  const [pending, setPending] = useState<{ name: string; provider: string } | null>(null);
  const [search, setSearch] = useState("");
  const internalRef = useRef<HTMLDivElement>(null);
  const ref = useMergedRef(internalRef, externalRef);

  // Calculate menu width based on the longest model row OR the longest
  // provider sticky header — whichever is wider. Provider moved to the
  // header so the row calc dropped its term, but the header itself
  // (logo + uppercase provider name + padding) can exceed the row for
  // long ids like `minimax-cn-coding-plan`.
  // Ponytail — ceiling: ~max(modelName * 7.5 + 86px, providerName * 7px + 42px).
  const menuWidth = useMemo(() => {
    const CHAR_WIDTH = 7.5; // px per char for text-xs (model rows)
    const HEADER_CHAR_WIDTH = 7.5; // px per char for text-10 uppercase + tracking-wide
    const PADDING = 24; // px-3 on each side
    const ROW_CHROME = 78; // leading logo + feature icons + check slot + chevron + gaps on model rows
    const HEADER_CHROME = 18; // 12 logo + 6 gap, before the provider name
    let maxWidth = 0;

    for (const m of models) {
      const displayName = m.name.includes('/') && m.name.split('/')[0].length < m.name.split('/').slice(1).join('/').length
        ? m.name.split('/').slice(1).join('/')
        : m.name;
      const rowWidth = displayName.length * CHAR_WIDTH + ROW_CHROME + PADDING;
      const headerWidth = HEADER_CHROME + m.provider.length * HEADER_CHAR_WIDTH + PADDING;
      maxWidth = Math.max(maxWidth, rowWidth, headerWidth);
    }

    return Math.max(maxWidth, 180); // Minimum 180px
  }, [models]);

  // Close on outside click — also resets drill-down state so the fly-out
  // doesn't reappear stale on the next open.
  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (internalRef.current && !internalRef.current.contains(e.target as Node)) {
        closeMenu();
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  // Above this many rows the list gets a filter box; below it the box is noise.
  const SEARCH_THRESHOLD = 8;

  const closeMenu = () => {
    cancelClose();
    setOpen(false);
    setPending(null);
    setSearch("");
  };

  // Hover-to-open plumbing for the account fly-out. A short delay gives
  // the cursor time to cross the gap between row and fly-out without
  // firing a close.
  const flyoutRef = useRef<HTMLDivElement>(null);
  const rowRefs = useRef<Map<string, HTMLButtonElement>>(new Map());
  const closeTimerRef = useRef<number | null>(null);
  const cancelClose = () => {
    if (closeTimerRef.current !== null) {
      window.clearTimeout(closeTimerRef.current);
      closeTimerRef.current = null;
    }
  };
  const scheduleClose = () => {
    cancelClose();
    closeTimerRef.current = window.setTimeout(() => {
      setPending(null);
      closeTimerRef.current = null;
    }, 120);
  };

  // Fly-out placement. Anchored to the row's viewport rect (so it stays
  // glued to the row when the list scrolls), positioned via fixed coords
  // so the menu's `overflow-hidden` doesn't clip it.
  // Direction = down if there's room below the row, else up. If neither
  // side fits, pick the larger one.
  const [flyoutPos, setFlyoutPos] = useState<{
    top: number;
    left: number;
    direction: "down" | "up";
  } | null>(null);

  const query = search.trim().toLowerCase();

  // Models grouped by provider, sorted alphabetically (case-insensitive)
  // by provider first, then by model name within each provider. See
  // `groupModelsByProvider` for why stable ordering is required here.
  const groups = useMemo(() => groupModelsByProvider(models, query), [models, query]);

  // `pending` now drives a side fly-out, not a panel swap — so the search
  // box stays scoped to the main model list, and `accounts` / `visibleAccounts`
  // are only consumed by the fly-out (which shows every account of the
  // picked model without re-applying the query).
  const accounts = pending ? (accountsByProvider[pending.provider] ?? []) : [];
  const visibleAccounts = accounts;
  const showSearch = models.length > SEARCH_THRESHOLD;

  // Fly-out placement effect — runs after `accounts` is in scope so the
  // height estimate is accurate. Anchored to the row's viewport rect so
  // it stays glued when the list scrolls. Direction picks the side with
  // enough room, else the larger side.
  useLayoutEffect(() => {
    if (!pending) {
      setFlyoutPos(null);
      return;
    }
    // Estimate fly-out height: ~36px per account row + ~50px chrome,
    // capped by the scrollable list's max-h-[420px]. If wrong by a few
    // px the next paint re-measures via the actual DOM rect.
    const flyoutHeight = Math.min(accounts.length * 36 + 50, 420);
    const compute = () => {
      const row = rowRefs.current.get(`${pending.name}::${pending.provider}`);
      if (!row) return;
      const rowRect = row.getBoundingClientRect();
      const spaceBelow = window.innerHeight - rowRect.bottom;
      const spaceAbove = rowRect.top;

      let direction: "down" | "up";
      let top: number;
      if (flyoutHeight <= spaceBelow) {
        direction = "down";
        top = rowRect.top;
      } else if (flyoutHeight <= spaceAbove) {
        direction = "up";
        top = rowRect.bottom - flyoutHeight;
      } else if (spaceBelow >= spaceAbove) {
        direction = "down";
        top = rowRect.top;
      } else {
        direction = "up";
        top = rowRect.bottom - flyoutHeight;
      }
      setFlyoutPos({ top, left: rowRect.right + 4, direction });
    };
    compute();
    window.addEventListener("scroll", compute, true);
    window.addEventListener("resize", compute);
    return () => {
      window.removeEventListener("scroll", compute, true);
      window.removeEventListener("resize", compute);
    };
  }, [pending, accounts.length, models]);

  // Last-used account per provider — display hint only, never a preselection
  // (the user always clicks). Local storage because it is pure UI memory of
  // this machine, not session state.
  const lastUsedAccount = (provider: string) => {
    try { return localStorage.getItem(`acowork:last-account:${provider}`); } catch { return null; }
  };

  const pickModel = (name: string, provider: string) => {
    const list = accountsByProvider[provider] ?? [];
    if (list.length > 1) {
      setPending({ name, provider });
      setSearch("");
      return;
    }
    onSelect(name, provider, list[0]?.accountId);
    closeMenu();
  };

  const pickAccount = (accountId: string) => {
    if (!pending) return;
    try { localStorage.setItem(`acowork:last-account:${pending.provider}`, accountId); } catch { /* ignore */ }
    onSelect(pending.name, pending.provider, accountId);
    closeMenu();
  };

  const modelDisplayName = (() => {
    if (!currentModel || !currentModel.includes('/')) return currentModel ?? t("chatPanel.modelFallback");
    const parts = currentModel.split('/');
    const prefix = parts[0];
    const modelName = parts.slice(1).join('/');
    return modelName.length > prefix.length ? modelName : currentModel;
  })();

  return (
    <ToolbarDropdownTrigger
      // The button shows a MODEL, so the icon is resolved from the model
      // name first (claude-* → anthropic, gemini-* → google, …) and only then
      // from the provider id. That ordering is what makes it correct for a
      // custom gateway serving several vendors' models, and for an aggregate
      // one where the gateway's own name says nothing. Falls back to Layers
      // only when there's no provider at all.
      icon={
        currentProvider ? (
          <ProviderLogo providerId={currentProvider} modelId={currentModel ?? undefined} size={14} />
        ) : (
          <Layers size={14} />
        )
      }
      label={modelDisplayName}
      collapseClass="tb-model-text"
      tipClass="tb-model-tip"
      tooltip={readOnly ? t("chatPanel.readOnlySession") : t("chatPanel.selectModel")}
      open={open}
      onToggle={() => !readOnly && (open ? closeMenu() : setOpen(true))}
      disabled={readOnly}
      wrapperRef={ref}
      textHidden={textHidden}
      btnId={btnId}
    >
      {/* Popup menu */}
      {open && (
        <>
        {/* Main panel — model list. Always shown. The account fly-out
            (below) anchors to the same `bottom-full` baseline, so the
            two panels share a top edge without manual offset maths. */}
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-modal-surface dark:border-border-outer",
          )}
          style={{ width: `${menuWidth}px` }}
        >
          {/* Menu title */}
          <div className="px-3 pt-2.5 pb-1">
            <h2 className="text-sm font-normal text-text-secondary ">
              {t("chatPanel.modelMenuTitle")}
            </h2>
          </div>

          {/* Filter — only worth showing once the list is long */}
          {showSearch && (
            <div className="px-3 pb-1.5">
              <div className="flex items-center gap-1.5 rounded-md border border-input-border bg-input-bg px-2 py-1">
                <Search size={11} className="shrink-0 text-text-tertiary" />
                <input
                  autoFocus
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                  placeholder={t("chatPanel.modelMenuSearchModel")}
                  className="w-full bg-transparent text-xs text-text-primary outline-none placeholder:text-text-tertiary"
                />
              </div>
            </div>
          )}

          {/* Divider between the filter box and the model list — same
              token as the provider-section dividers below. */}
          {showSearch && <div className="border-t border-border-divider" />}

          {/* Level 1: grouped model list (level 2 — accounts — flies out on
              the right; this panel no longer swaps to show it). */}
          <div className="max-h-[420px] overflow-y-auto py-1">
            {groups.length === 0 ? (
              <div className="px-3 py-2 text-xs text-text-tertiary">{t("chatPanel.modelMenuNoMatch")}</div>
            ) : (
              groups.map(([provider, providerModels], groupIdx) => (
                <div key={provider}>
                  {/* Divider between provider sections (color from --color-border-divider).
                      Suppressed for the first group so the menu top edge stays flush. */}
                  {groupIdx > 0 && <div className="border-t border-border-divider" />}
                  {/* Sticky so the provider stays visible while its models scroll */}
                  <div className="sticky top-0 z-10 flex items-center gap-1.5 bg-modal-surface px-3 py-1 text-10 font-semibold uppercase tracking-wide text-text-tertiary">
                    {/* Provider section header — no modelId on purpose: the
                        header names the *provider*, so it shows the provider's
                        own mark (or nothing). Only the trigger, which shows a
                        single selected model, resolves through the model name. */}
                    <ProviderLogo providerId={provider} size={12} />
                    <span className="truncate">{provider}</span>
                  </div>
                  {providerModels.map((m) => {
                    const isActive = m.name === currentModel && m.provider === currentProvider;
                    const drillsDown = (accountsByProvider[m.provider]?.length ?? 0) > 1;
                    return (
                      <button
                        key={`${m.name}::${m.provider}`}
                        type="button"
                        ref={(el) => {
                          const k = `${m.name}::${m.provider}`;
                          if (el) rowRefs.current.set(k, el);
                          else rowRefs.current.delete(k);
                        }}
                        onClick={() => pickModel(m.name, m.provider)}
                        onMouseEnter={
                          drillsDown
                            ? () => {
                                cancelClose();
                                setPending({ name: m.name, provider: m.provider });
                              }
                            : undefined
                        }
                        onMouseLeave={drillsDown ? scheduleClose : undefined}
                        className={cn(
                          "flex w-full items-center justify-between pl-5 pr-3 py-1.5 text-xs font-medium transition-colors",
                          isActive ? menuItemSelected : menuItemHover,
                        )}
                      >
                        <span className="flex items-center gap-1 min-w-0">
                          {/* Row shows a model, so the mark resolves from the model
                              name first (same rule as the trigger button) — a
                              custom gateway serving several vendors gets a
                              distinct mark per row instead of one wrong mark for
                              all of them. */}
                          <ProviderLogo providerId={m.provider} modelId={m.name} size={12} />
                          <span className="font-medium truncate">
                            {/* Strip provider prefix from model name if format is provider/model and model is longer */}
                            {(() => {
                              if (!m.name.includes('/')) return m.name;
                              const parts = m.name.split('/');
                              const prefix = parts[0];
                              const modelName = parts.slice(1).join('/');
                              // Only strip if model name is longer than prefix (avoid stripping model/provider)
                              return modelName.length > prefix.length ? modelName : m.name;
                            })()}
                          </span>
                          <span className="flex items-center gap-0.5 ml-2">
                            {m.tool_call && <Wrench size={10} className="text-text-tertiary" />}
                            {m.reasoning && <Brain size={10} className="text-purple-400" />}
                            {m.input_modalities?.includes('image') && <Image size={10} className="text-blue-400" />}
                          </span>
                        </span>
                        {/* Trailing cluster. The check box is a fixed w-3 slot
                            whenever a chevron is possible, so every drill-down
                            row's chevron lands on the same `pr-3` baseline
                            whether or not the check inside it is filled —
                            otherwise the active row's chevron shifts left. */}
                        <span className="ml-2 flex shrink-0 items-center gap-1">
                          {(isActive || drillsDown) && (
                            <span className="flex w-3 shrink-0 justify-end">
                              {isActive && <Check size={12} className="text-[var(--color-accent)]" />}
                            </span>
                          )}
                          {drillsDown && <ChevronRight size={12} className="text-text-tertiary" />}
                        </span>
                      </button>
                    );
                  })}
                </div>
              ))
            )}
          </div>

          {/* Divider */}
          <div className="border-t border-border-divider" />

          {/* Add Models button — same style as Install Agent */}
          <button
            type="button"
            onClick={() => {
              setShowAddDialog(true);
              closeMenu();
            }}
            className="mx-3 mt-2 mb-2.5 flex w-[calc(100%-1.5rem)] items-center justify-center gap-1.5 rounded-md bg-zinc-100 px-3 py-[var(--ui-btn-py)] text-xs font-medium text-text-secondary transition-colors hover:bg-zinc-200 hover:text-zinc-900 dark:bg-white/10  dark:hover:bg-white/15 dark:hover:text-zinc-100"
          >
            <Plus className="h-3.5 w-3.5" />
            {t("chatPanel.addModel")}
          </button>
        </div>

        {/* Level 2 fly-out — accounts for the multi-key model the user
            drilled into. Anchored to the same `bottom-full` baseline as
            the main panel so the two top edges align without manual
            offset. Rendered as a sibling (not a child) so the main
            panel's `overflow-hidden` doesn't clip it. No back button:
            the main panel stays visible and clicking another multi-key
            row swaps this fly-out to the new model. */}
        {pending && (
          <div
            ref={flyoutRef}
            onMouseEnter={cancelClose}
            onMouseLeave={scheduleClose}
            className="fixed z-50 overflow-hidden rounded-md border shadow-lg border-zinc-200 bg-modal-surface dark:border-border-outer"
            style={{
              top: flyoutPos?.top ?? 0,
              left: flyoutPos?.left ?? 0,
              width: "200px",
              visibility: flyoutPos ? "visible" : "hidden",
            }}
          >
            <div className="max-h-[420px] overflow-y-auto py-1.5">
              {visibleAccounts.length === 0 ? (
                <div className="px-3 py-2 text-xs text-text-tertiary">{t("chatPanel.modelMenuNoMatch")}</div>
              ) : (
                visibleAccounts.map((a) => {
                  const isActive =
                    a.accountId === currentAccountId &&
                    pending.name === currentModel &&
                    pending.provider === currentProvider;
                  const isLastUsed = lastUsedAccount(pending.provider) === a.accountId;
                  return (
                    <button
                      key={a.accountId}
                      type="button"
                      onClick={() => pickAccount(a.accountId)}
                        className={cn(
                          "flex w-full items-center justify-between px-3 py-1.5 text-xs font-medium transition-colors",
                          isActive ? menuItemSelected : menuItemHover,
                        )}
                      >
                        <span className="flex min-w-0 items-center gap-1">
                          <span className="truncate font-medium">{a.alias}</span>
                        {isLastUsed && !isActive && (
                          <span
                            title={t("chatPanel.modelMenuLastUsed")}
                            aria-label={t("chatPanel.modelMenuLastUsed")}
                            className="inline-flex shrink-0 items-center text-[var(--color-accent)]"
                          >
                            <Clock size={10} />
                          </span>
                        )}
                      </span>
                      <span className="ml-2 flex shrink-0 items-center gap-1">
                        <span className="text-10 text-text-tertiary">{a.preview}</span>
                        {isActive && <Check size={12} />}
                      </span>
                    </button>
                  );
                })
              )}
            </div>
          </div>
        )}
        </>
      )}

      {/* Add Provider Flow */}
      <AddProviderFlow
        open={showAddDialog}
        onClose={() => setShowAddDialog(false)}
        onSuccess={() => {
          window.dispatchEvent(new Event('models-added'));
        }}
      />
    </ToolbarDropdownTrigger>
  );
}

/** Reasoning effort selector — popup with Auto/Off/Low/Medium/High */
function ReasoningEffortMenu({
  effort,
  onChange,
  textHidden,
  wrapperRef: externalRef,
  btnId,
  readOnly,
}: {
  effort: string | null;
  onChange: (effort: string) => void;
  textHidden?: boolean;
  /** Optional external ref merged with the internal click-outside ref */
  wrapperRef?: React.Ref<HTMLDivElement>;
  /** Toolbar button id used by ChatPanel's collapse observer */
  btnId?: string;
  /** ADR-076 §决策 4: session is not writable by the signed-in account. */
  readOnly?: boolean;
}) {
  const { t } = useTranslation();
  const [open, setOpen] = useState(false);
  const internalRef = useRef<HTMLDivElement>(null);
  const ref = useMergedRef(internalRef, externalRef);

  // Values are lowercase to match backend ReasoningEffort serde serialization.
  const OPTIONS: { value: string; label: string; color: string }[] = [
    { value: "auto", label: t("chatPanel.reasoningAuto"), color: "#22c55e" },
    { value: "off", label: t("chatPanel.reasoningOff"), color: "#9ca3af" },
    { value: "low", label: t("chatPanel.reasoningLow"), color: "#3b82f6" },
    { value: "medium", label: t("chatPanel.reasoningMedium"), color: "#8b5cf6" },
    { value: "high", label: t("chatPanel.reasoningHigh"), color: "#ef4444" },
  ];

  const currentOpt = OPTIONS.find((o) => o.value === effort);
  const effortLabel = currentOpt?.label ?? "Auto";
  const currentColor = currentOpt?.color ?? "#22c55e";

  useEffect(() => {
    if (!open) return;
    const handler = (e: MouseEvent) => {
      if (internalRef.current && !internalRef.current.contains(e.target as Node)) {
        setOpen(false);
      }
    };
    document.addEventListener("mousedown", handler);
    return () => document.removeEventListener("mousedown", handler);
  }, [open]);

  return (
    <ToolbarDropdownTrigger
      icon={<Brain size={14} style={{ color: currentColor }} />}
      label={effortLabel}
      collapseClass="tb-effort-text"
      tipClass="tb-effort-tip"
      tooltip={readOnly ? t("chatPanel.readOnlySession") : (t("chatPanel.selectReasoningEffort") ?? "Reasoning effort")}
      open={open}
      onToggle={() => !readOnly && setOpen(!open)}
      disabled={readOnly}
      wrapperRef={ref}
      textHidden={textHidden}
      btnId={btnId}
    >
      {open && (
        <div
          className={cn(
            "absolute bottom-full left-0 z-50 mb-1 overflow-hidden rounded-md border shadow-lg",
            "border-zinc-200 bg-modal-surface dark:border-border-outer",
          )}
          style={{ width: "140px" }}
        >
          {/* Menu title */}
          <div className="px-3 pt-2.5 pb-1">
            <h2 className="text-sm font-normal text-text-secondary ">
              {t("chatPanel.reasoningMenuTitle")}
            </h2>
          </div>

          {/* Options */}
          <div className="py-1">
            {OPTIONS.map((opt) => {
              const isActive = opt.value === effort;
              return (
                <button
                  key={opt.value}
                  type="button"
                  onClick={() => {
                    onChange(opt.value);
                    setOpen(false);
                  }}
                  className={cn(
                    "flex w-full items-center gap-2 px-3 py-1.5 text-xs font-medium transition-colors",
                    isActive ? menuItemSelected : menuItemHover,
                  )}
                >
                  <span
                    className="h-2 w-2 rounded-full shrink-0"
                    style={{ backgroundColor: opt.color }}
                  />
                  <span className="font-medium">{opt.label}</span>
                  {isActive && <Check size={12} className="ml-auto text-[var(--color-accent)]" />}
                </button>
              );
            })}
          </div>
        </div>
      )}
    </ToolbarDropdownTrigger>
  );
}
