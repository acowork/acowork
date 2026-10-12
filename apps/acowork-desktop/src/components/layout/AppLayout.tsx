import { useState, useCallback, useEffect, useMemo, useRef, lazy, Suspense } from "react";
import { invoke } from "@tauri-apps/api/core";
import type { NavView } from "../../lib/types";
import { NavBar } from "./NavBar";
import { TitleBar } from "./TitleBar";
import { AgentList } from "../agent-list/AgentList";
import { ChatPanel } from "../chat/ChatPanel";
import { EmptyChatPane } from "../chat/EmptyChatPane";
import { InboxPanel } from "../../views/InboxPanel";
import { RightPanel } from "../right-panel/RightPanel";
import { RightNavBar } from "./RightNavBar";
// FileEditorPanel is lazy-loaded: its module graph pulls in monaco-editor
// (~170 kB) plus monaco-languageclient's vscode-api stack (~1 MB) via
// useLspClientPool. Keeping it out of the entry chunk is what actually
// moves monaco off the first-paint path — a top-level import here would
// defeat initMonaco() in lib/monacoBootstrap.ts.
//
// Kept as a factory rather than a module-level `lazy()`: React remembers a
// rejected import for the lifetime of the lazy component, so retrying after
// a failed load requires building a new one (see editorLoadAttempt below).
const loadFileEditorPanel = () =>
    import("../editor/FileEditorPanel").then((m) => ({ default: m.FileEditorPanel }));
import {
  useGatewayStore,
  cancelHealthProbe,
  markGatewayAlive,
  startGatewayDeathWatch,
  stopGatewayDeathWatch,
  type GatewayAlive,
} from "../../stores/gatewayStore";
import { applyGatewayTransition, onMqttConnectionEdge } from "./gatewayTransition";
import { useSettingsStore } from "../../stores/settingsStore";
import { useAgentStore } from "../../stores/agentStore";
import { useServicesStore } from "../../stores/servicesStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { useStatusBarStore } from "../../stores/statusBarStore";
import { useEditorStatusStore } from "../../stores/editorStatusStore";
import { FileStatusCluster, type FileStatusClusterActiveFile } from "../editor/FileStatusCluster";
import { cn, formatPercent } from "../../lib/utils";
import { computeCacheHitRate, formatCacheHitRate, hasCacheData } from "../../lib/cacheHitRate";
import {
  DEFAULT_ACCENT_PRESET,
  getAccentPresetByHex,
} from "../../lib/accentPresets";
import { SettingsPage } from "../settings/SettingsPage";
import { useLocalNodeAutoStart } from "../../hooks/useLocalNodeAutoStart";
import { HarnessPage } from "../harness/HarnessPage";
import { ProjectsView } from "../../views/ProjectsView";

import { DocsView } from "../../views/DocsView";
import { ExtensionsView } from "../../views/ExtensionsView";
import { MqttDebugControls } from "../debug/MqttDebugControls";
import { Tooltip } from "../common/Tooltip";
import { useChatStore } from "../../stores/chatStore";
import { useLayoutStore } from "../../stores/layoutStore";
import { useUserChatStore } from "../../stores/userChatStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useSearchStore, isGlobalSearchShortcut } from "../../stores/searchStore";
import { GlobalSearchDialog } from "../search/GlobalSearchDialog";
import { useTranslation } from "../../i18n/useTranslation";
import { AlertTriangle, Bot, Check, Cpu, RefreshCw } from "lucide-react";
import { ChunkLoadBoundary } from "../common/ErrorBoundary";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { log } from "../../lib/logger";
// Pane floors and the overflow ladder live in panelClamp.ts so the drag
// ceiling, the effect below and the tests all rank the same numbers.
import { CHROME_WIDTH, MIN_CHAT_WIDTH, MIN_FILE_WIDTH, MIN_RIGHT_WIDTH, fitPanesToViewport } from "./panelClamp";

/** Settings tab type — keep in sync with SettingsPage */
type SettingsTab = "gateway" | "appearance" | "general" | "profile";
const MIN_SIDEBAR_WIDTH = 100;
const AVATAR_SIDEBAR_WIDTH = 64;
const MAX_SIDEBAR_WIDTH = 400;
const DEFAULT_SIDEBAR_WIDTH = 240;
const SIDEBAR_WIDTH_KEY = "acowork-sidebar-width";

const MAX_RIGHT_WIDTH = 600;
const DEFAULT_RIGHT_WIDTH = 340;
const RIGHT_WIDTH_KEY = "acowork-right-width";

const MAX_FILE_WIDTH = 900;
const DEFAULT_FILE_WIDTH = 450;
const FILE_WIDTH_KEY = "acowork-file-width";

export function AppLayout() {
  // ADR-087 follow-up: remote mode — resume this machine's enrolled local
  // Node on launch (silent; first-time enrollment stays an explicit
  // action via the AgentList banner).
  useLocalNodeAutoStart();

  // Editor panel load failures stay local: a failed dynamic import used to
  // reject through the app-level ErrorBoundary (App.tsx) and blank the whole
  // window. Bumping the attempt builds a fresh lazy component and remounts
  // the boundary via `key` — the only way to actually re-run the import.
  const [editorLoadAttempt, setEditorLoadAttempt] = useState(0);
  const FileEditorPanel = useMemo(() => lazy(loadFileEditorPanel), [editorLoadAttempt]);

  // Pull the editor chunk into memory once the first frame is on screen, so
  // opening a file never has to fetch it later. In dev that fetch is an HTTP
  // round trip to the dev server — which is what made "click a markdown file"
  // fail whenever the server was gone (a stale tray window outliving the dev
  // stack). In production it is a 7.7 MB parse paid at click time. rAF +
  // setTimeout lands this after the first paint, so first paint is unaffected.
  //
  // Safe to do in both modes: the panel's module graph has no top-level side
  // effects (no LSP client, no worker, nothing spawned at import time), and a
  // failed prefetch is swallowed — the lazy import below stays the real load
  // path, so this can only make loading earlier, never differently.
  useEffect(() => {
    let timer = 0;
    const warm = () => void loadFileEditorPanel().catch(() => {});
    const raf = requestAnimationFrame(() => {
      timer = window.setTimeout(warm, 0);
    });
    return () => {
      cancelAnimationFrame(raf);
      if (timer) window.clearTimeout(timer);
    };
  }, []);

  const [currentView, setCurrentView] = useState<NavView>("chat");
  const [settingsInitialTab, setSettingsInitialTab] = useState<SettingsTab>("gateway");
  const activeTab = useLayoutStore((s) => s.activePanelTab);
  const setActiveTab = useLayoutStore((s) => s.setActivePanelTab);
  const rightPanelCollapsed = useLayoutStore((s) => s.rightPanelCollapsed);
  const setRightPanelCollapsed = useLayoutStore((s) => s.setRightPanelCollapsed);
  const [sidebarWidth, setSidebarWidth] = useState(() => {
    const stored = localStorage.getItem(SIDEBAR_WIDTH_KEY);
    if (stored) {
      const val = parseInt(stored, 10);
      if (val <= AVATAR_SIDEBAR_WIDTH) return AVATAR_SIDEBAR_WIDTH;
      return Math.min(val, MAX_SIDEBAR_WIDTH);
    }
    return DEFAULT_SIDEBAR_WIDTH;
  });
  const [rightWidth, setRightWidth] = useState(() => {
    const stored = localStorage.getItem(RIGHT_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH) : DEFAULT_RIGHT_WIDTH;
  });
  const [fileWidth, setFileWidth] = useState(() => {
    const stored = localStorage.getItem(FILE_WIDTH_KEY);
    return stored ? Math.min(Math.max(parseInt(stored, 10), MIN_FILE_WIDTH), MAX_FILE_WIDTH) : DEFAULT_FILE_WIDTH;
  });
  const hasOpenFiles = useFileEditorStore((s) => s.openFiles.length > 0);
  const fileWidthInitialized = useRef(false);

  // [DEBUG] Mount diagnostic — print state immediately after webview reload
  const mountLoggedRef = useRef(false);
  useEffect(() => {
    if (mountLoggedRef.current) return;
    mountLoggedRef.current = true;
    const a = useAgentStore.getState();
    const c = useChatStore.getState();
    const sid = a.selectedAgentId ?? "";
    log.debug("[AppLayout] MOUNT", {
      selectedAgentId: a.selectedAgentId,
      activeSessionId: c.agentStates[sid]?.activeSessionId ?? null,
      openSessionIds: c.agentStates[sid]?.openSessionIds ?? [],
      knownAgents: Object.keys(a.agents),
      sessionsForSelected: a.selectedAgentId ? (a.agents[a.selectedAgentId]?.sessions ?? []).map((s) => s.session_id) : null,
    });
  }, []);

  // Ensure agents are fetched on mount (AgentList already does this via its
  // own useEffect, but AppLayout may mount before AgentList, so we also
  // trigger it here as a guard.  fetchAgents internally does auto-select +
  // loadLatestSession, so no extra recovery logic is needed.)
  const fetchedRef = useRef(false);
  useEffect(() => {
    if (fetchedRef.current) return;
    fetchedRef.current = true;
    const store = useAgentStore.getState();
    if (Object.keys(store.agents).length === 0) {
      void store.fetchAgents().then(() => {
        log.debug("[AppLayout] initial fetchAgents complete", {
          selectedAgentId: store.selectedAgentId,
          knownAgents: Object.keys(store.agents),
        });
      });
    }
  }, []);

  // ADR-XXX: Presence heartbeat for the idle-watcher. The selected
  // Auto-sleep was retired in Sept 2026 — the ActiveHeartbeat loop is
  // gone. The selected agent's Runtime stays alive for as long as the
  // user keeps it running; stop / start is the only transition.

  // Refs to track latest panel widths for proportional window-resize scaling
  const fileWidthValueRef = useRef(fileWidth);
  fileWidthValueRef.current = fileWidth;
  const sidebarWidthRef = useRef(sidebarWidth);
  sidebarWidthRef.current = sidebarWidth;
  const rightWidthRef = useRef(rightWidth);
  rightWidthRef.current = rightWidth;
  const rightPanelCollapsedRef = useRef(rightPanelCollapsed);
  rightPanelCollapsedRef.current = rightPanelCollapsed;

  // Auto-size file panel to half available area on first open
  useEffect(() => {
    if (hasOpenFiles && !fileWidthInitialized.current) {
      fileWidthInitialized.current = true;
      const available = window.innerWidth - CHROME_WIDTH - sidebarWidth - (rightPanelCollapsed ? 0 : rightWidth);
      const halfWidth = Math.min(Math.max(Math.round(available / 2), MIN_FILE_WIDTH), MAX_FILE_WIDTH);
      // Always recalculate on first open to respect current window size,
      // preventing the stored width from obscuring the session panel
      setFileWidth(halfWidth);
      localStorage.setItem(FILE_WIDTH_KEY, String(halfWidth));
    }
    if (!hasOpenFiles) {
      fileWidthInitialized.current = false;
    }
  }, [hasOpenFiles, sidebarWidth, rightWidth, rightPanelCollapsed]);

  const gatewayStatus = useGatewayStore((s) => s.status);
  const gatewayAlive = useGatewayStore((s) => s.gatewayAlive);
  const checkHealth = useGatewayStore((s) => s.checkHealth);
  const setStatus = useStatusBarStore((s) => s.setStatus);
  const statusMsg = useStatusBarStore((s) => s.message);
  const statusType = useStatusBarStore((s) => s.type);
  const statusVisible = useStatusBarStore((s) => s.visible);
  const clearStatus = useStatusBarStore((s) => s.clearStatus);

  // ── File-status cluster subscriptions (PR-2 of the unified status-bar refactor) ──
  // Read the live bounding rect reported by `useReportFilePanelBounds`
  // (mounted inside `FileEditorPanel`). Use individual field selectors so
  // unrelated components do not re-render on every state mutation — e.g.
  // each Monaco cursor move updates only `cursor`, not the LSP signals.
  const filePanelBounds = useLayoutStore((s) => s.filePanelBounds);
  const editorCursor = useEditorStatusStore((s) => s.cursor);
  const editorSelectedCount = useEditorStatusStore((s) => s.selectedCount);
  const editorLspEnabled = useEditorStatusStore((s) => s.lspEnabled);
  const editorLspLanguage = useEditorStatusStore((s) => s.lspLanguage);
  const editorLspStatus = useEditorStatusStore((s) => s.lspStatus);
  const editorLspStatusMessage = useEditorStatusStore((s) => s.lspStatusMessage);

  // Active file — pulled from `fileEditorStore` and trimmed to the cluster's
  // minimal structural shape so the cluster does not have to import the
  // full `OpenFile` type.
  const openFiles = useFileEditorStore((s) => s.openFiles);
  const activeFileId = useFileEditorStore((s) => s.activeFileId);
  const clusterActiveFile: FileStatusClusterActiveFile | null =
    activeFileId !== null
      ? (() => {
            const f = openFiles.find((file) => file.id === activeFileId);
            if (!f) return null;
            return {
                fileName: f.fileName,
                language: f.language,
                mimeType: f.mimeType,
                mode: f.mode,
                kind: f.kind,
                url: f.url,
                relPath: f.relPath,
                loading: f.loading,
                agentId: f.agentId,
            };
        })()
      : null;

  // ── Window width tracking ────────────────────────────────────────────
  // The cluster is positioned absolutely inside the global bar with
  // `right: ${windowW - bounds.right}px`, so we need the live window width
  // in pixels. Tauri webviews fire `resize` on the window object (not just
  // `window` resize) — we listen on both. Throttled with `requestAnimationFrame`
  // to keep drag-to-resize cheap.
  const [windowWidth, setWindowWidth] = useState<number>(
    () => (typeof window !== "undefined" ? window.innerWidth : 0),
  );
  useEffect(() => {
    if (typeof window === "undefined") return;
    let rafId: number | null = null;
    const schedule = () => {
      if (rafId !== null) return;
      rafId = requestAnimationFrame(() => {
        rafId = null;
        setWindowWidth(window.innerWidth);
      });
    };
    window.addEventListener("resize", schedule);
    return () => {
      window.removeEventListener("resize", schedule);
      if (rafId !== null) cancelAnimationFrame(rafId);
    };
  }, []);
  // Determine if selected agent is in debug mode.
  // ADR-048 follow-up: key off `debug_state` (current capability), not
  // `dev_mode` (startup intent) — DevMode can be flipped on at runtime
  // via POST /api/agents/{id}/debug/enable without restarting the agent.
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agents = useAgentStore((s) => s.agents);
  const selectedAgent = selectedAgentId ? (agents[selectedAgentId]?.meta ?? null) : null;
  const isDebugMode = selectedAgent?.debug_state === "enabled" && selectedAgent?.alive;
  // Width the right panel actually occupies in the chat-view row. Mirrors the
  // render condition on `<RightPanel>` below (`!rightPanelCollapsed &&
  // selectedAgentId`) — counting it while it is not mounted would clamp the
  // file pane against ~340px that nothing is using.
  const rightPaneWidth = !rightPanelCollapsed && selectedAgentId ? rightWidth : 0;
  // Middle-panel routing: agent session wins; otherwise an open inbox thread
  // takes the pane. An explicit empty state at the bottom handles "neither".
  const activePeerId = useUserChatStore((s) => s.activePeerId);
  // ADR-009 §V-Q: the name is server-side (Runtime `.overrides.json`,
  // mirrored into the Gateway's list view) — no local override.
  const agentDisplayName = selectedAgent
    ? (selectedAgent.display_name ?? selectedAgent.name)
    : null;
  // Context usage for the bottom status bar.
  // (Session count was removed — see PR-3 follow-up: users found it noisy
  // without adding actionable signal beyond the active session indicator
  // already present in the chat panel tab bar.)
  const contextUsage = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.contextUsage ?? null;
  });
  // ADR-066: provider for the active session — needed to pick the
  // cache-hit-ratio formula (Anthropic vs OpenAI) for the status bar.
  const sessionProvider = useChatStore((s) => {
    if (!selectedAgentId) return null;
    const agent = s.agentStates[selectedAgentId];
    if (!agent?.activeSessionId) return null;
    return agent.sessionStates[agent.activeSessionId]?.provider ?? null;
  });
  // ADR-066 §6: cache hit rate for the status bar.  The status bar is a
  // session-level surface, so it shows the session-lifetime (cumulative)
  // rate — the input-box popover shows the per-turn rate instead.
  // `null` means "no signal" — provider doesn't report cache tokens, no
  // LLM call yet, or a zero denominator — in which case we hide the pill
  // entirely.
  const cacheHitRateLabel = formatCacheHitRate(
    computeCacheHitRate(sessionProvider, contextUsage, "cumulative"),
  );
  // ADR-036: MQTT connection liveness is pushed from Rust via the
  // `mqtt-status` Tauri event.  The Rust eventloop owns reconnection —
  // we just reflect state into the UI.
  const mqttConnected = useChatStore((s) => s.mqttConnected);
  const lastMqttError = useChatStore((s) => s.lastMqttError);
  // Status message click-to-copy: short-lived "Copied!" feedback.
  // Reset whenever the underlying message changes so a new warning
  // arriving mid-feedback doesn't leave a stale checkmark.
  const [statusCopied, setStatusCopied] = useState(false);
  const statusCopyTimerRef = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => {
    setStatusCopied(false);
    if (statusCopyTimerRef.current) {
      clearTimeout(statusCopyTimerRef.current);
      statusCopyTimerRef.current = null;
    }
  }, [statusMsg]);
  useEffect(() => {
    return () => {
      if (statusCopyTimerRef.current) clearTimeout(statusCopyTimerRef.current);
    };
  }, []);
  const handleCopyStatusMsg = useCallback(() => {
    if (!statusMsg) return;
    void navigator.clipboard.writeText(statusMsg)
      .then(() => {
        setStatusCopied(true);
        if (statusCopyTimerRef.current) clearTimeout(statusCopyTimerRef.current);
        statusCopyTimerRef.current = setTimeout(() => setStatusCopied(false), 1500);
      })
      .catch(() => {
        // Clipboard API can fail in some sandboxed contexts (e.g. a
        // future hardened WKWebView policy); fail silently — the
        // hover Tooltip still surfaces the full message.
      });
  }, [statusMsg]);

  const { t } = useTranslation();

  // ── Glass tint color ──────────────────────────────────────────────
  // Read both `theme` and `osTheme` from the store. The store keeps
  // `osTheme` in sync with macOS appearance via a matchMedia listener
  // (see settingsStore.ts), so re-renders here happen automatically when
  // the user switches dark/light while the app is running.
  //
  // The blur itself is rendered by the OS-native effect layer
  // (NSVisualEffectView on macOS, DWM Acrylic on Windows — configured via
  // `set_effects` in src-tauri/src/lib.rs).  This layer only paints the
  // tint; when opacity=0, glassBg becomes rgba(...,0) and the native
  // vibrancy shows through.  Do NOT add `backdrop-filter: blur(...)` here:
  // it operates within the CSS stacking context and would blur whatever
  // sits behind this div (i.e. the html placeholder color), not the
  // native layer below the WKWebView.
  //
  // NOTE: macOS NSVisualEffectView with UnderWindowBackground material
  // is always light (Apple's design), which washes out dark mode when
  // the CSS tint-layer opacity is low.  We therefore swap the native
  // effect material to match the theme — see the `set_window_effect`
  // effect below.  Light mode keeps UnderWindowBackground (light tint);
  // dark mode switches to Effect::Dark (dark tint).
  //
  // Even with Effect::Dark the native NSVisualEffectView is still a
  // translucent material that blends with desktop wallpaper.  When the
  // opacity slider is dragged to 0 the CSS tint layer disappears,
  // exposing whatever sits behind the window.  On a light desktop
  // wallpaper this makes the window appear whitish in dark mode.
  //
  // The set_window_effect command applies a native color tint as the
  // first line of defense.  As a second layer we also enforce minimum
  // CSS opacity floors for both light and dark modes on macOS so there
  // is always enough tint to keep the visual appearance consistent
  // regardless of desktop content.
  //
  // Windows DWM Acrylic does NOT have this issue — it
  // faithfully blurs the desktop content in both light and dark modes.
  // Applying the floor on Windows would make the glass effect
  // unnecessarily opaque, so we skip it on non-macOS platforms.
  const isMacOS =
    typeof navigator !== "undefined" && /Mac/i.test(navigator.userAgent);
  const LIGHT_OPACITY_FLOOR = 0.18;
  const DARK_OPACITY_FLOOR = 0.10;
  const { opacity, theme, osTheme, accentColor } = useSettingsStore();
  const isDark = theme === "dark" || (theme === "system" && osTheme === "dark");
  const lightOpacity = isMacOS
    ? Math.max(opacity, LIGHT_OPACITY_FLOOR)
    : opacity;
  const darkOpacity = isMacOS
    ? Math.max(opacity, DARK_OPACITY_FLOOR)
    : opacity;
  // CSS-layer glass tint.  The actual HSL components live in globals.css
  // and shift with the active `.accent-{id}` class on <html> (set by
  // settingsStore), so reading the CSS variable here means the surface
  // hue automatically follows the accent preset the user picks — at 8%
  // saturation the tint is almost neutral but distinct from pure white/
  // black desktop wallpaper.
  const glassBg = isDark
    ? `hsl(var(--glass-tint-dark) / ${darkOpacity})`
    : `hsl(var(--glass-tint-light) / ${lightOpacity})`;

  // ── Sync macOS native visual-effect material with theme + accent ──
  // The Rust setup() no longer applies an initial effect (to avoid
  // a race where its delayed retry loop clobbers this theme-aware
  // call).  This effect is the sole owner of the native material.
  // We retry up to 3 times with exponential backoff in case the
  // NSView hierarchy isn't ready yet when React first mounts.
  // Non-macOS: no-op.
  //
  // The effect is keyed on BOTH `isDark` and `accentColor` so that
  // changing either triggers a re-invoke — the native vibrancy tint
  // (RGBA tuple passed to Rust) needs to follow accent changes too,
  // otherwise the CSS-layer tint updates but the layer beneath the
  // WebView stays neutral, defeating the purpose at opacity=0.
  //
  // lastAppliedKeyRef guards against double-invocation: React StrictMode
  // (dev only) and React 18 concurrent rendering can both run this effect
  // twice for the same (isDark, accentId) pair.  The native
  // `set_window_effect` is idempotent so this is not a correctness issue,
  // but a duplicate call shows up in the console twice and obscures the
  // real boot-time signal.  We only skip when the previous run for this
  // exact key actually succeeded; failures do NOT update the ref so the
  // retry path still works on the next attempt.
  const lastAppliedKeyRef = useRef<string | null>(null);
  useEffect(() => {
    if (typeof navigator === "undefined" || !/Mac/i.test(navigator.userAgent)) return;

    const preset = getAccentPresetByHex(accentColor) ?? DEFAULT_ACCENT_PRESET;
    const key = `${isDark ? "dark" : "light"}:${preset.id}`;
    if (lastAppliedKeyRef.current === key) return;

    let cancelled = false;
    const tryApply = async (attempt: number) => {
      try {
        await invoke("set_window_effect", {
          isDark,
          lightRgba: [
            preset.glassRgbLight.r,
            preset.glassRgbLight.g,
            preset.glassRgbLight.b,
            30, // alpha — matches the original UnderWindowBackground tint
          ],
          darkRgba: [
            preset.glassRgbDark.r,
            preset.glassRgbDark.g,
            preset.glassRgbDark.b,
            40, // alpha — matches the original Effect::Dark tint
          ],
        });
        if (cancelled) return;
        log.debug(
          `[vibrancy] set_window_effect(${preset.id}, isDark=${isDark}) succeeded (attempt ${attempt})`,
        );
        lastAppliedKeyRef.current = key;
      } catch (e: unknown) {
        log.warn(`[vibrancy] set_window_effect attempt ${attempt} failed:`, e);
        if (!cancelled && attempt < 3) {
          const delay = 200 * Math.pow(2, attempt - 1); // 200, 400, 800 ms
          setTimeout(() => tryApply(attempt + 1), delay);
        }
      }
    };
    tryApply(1);

    return () => { cancelled = true; };
  }, [isDark, accentColor]);

  // ── Switch to debug tab when entering debug mode ─────────────────
  const prevIsDebugMode = useRef(isDebugMode);
  useEffect(() => {
    if (isDebugMode && !prevIsDebugMode.current) {
      setActiveTab("debug");
    }
    prevIsDebugMode.current = isDebugMode;
  }, [isDebugMode]);

  // ── Sync right-panel tab to agent lifecycle ──────────────────────
  // Stop: bounce back to status. RightNavBar hides the workspace / memory
  // / setup / tools / debug buttons once the agent stops, so leaving any
  // of them active would render an empty-state shell.
  // Start: jump to workspace. Workspace is the most common destination
  // after a launch (file edits, skill browsing, locate-in-tree), and the
  // user typically leaves status on the offline screen only until the
  // agent actually comes up. ADR follow-up: keeps nav and panel
  // consistent in both directions.
  const prevAlive = useRef(selectedAgent?.alive);
  useEffect(() => {
    const isAlive = selectedAgent?.alive ?? false;
    const wasAlive = prevAlive.current;
    if (isAlive && wasAlive === false) {
      setActiveTab("workspace");
    } else if (
      !isAlive &&
      wasAlive !== false &&
      (activeTab === "memory" ||
        activeTab === "setup" ||
        activeTab === "tools" ||
        activeTab === "debug" ||
        activeTab === "workspace")
    ) {
      setActiveTab("status");
    }
    prevAlive.current = isAlive;
  }, [selectedAgent?.alive, activeTab]);

  // ── Reveal workspace panel on locate-in-tree requests ────────────
  // The FileEditorPanel's "locate" button publishes a request via
  // workspaceStore.requestLocate; here we ensure the right-side
  // panel is expanded and the workspace tab is active so the user can
  // actually see the revealed file.
  const locateRequest = useWorkspaceStore((s) => s.locateRequest);
  const consumedLocateSeqRef = useRef<number>(-1);
  useEffect(() => {
    if (!locateRequest) return;
    if (locateRequest.seq <= consumedLocateSeqRef.current) return;
    consumedLocateSeqRef.current = locateRequest.seq;
    setRightPanelCollapsed(false);
    setActiveTab("workspace");
  }, [locateRequest, setRightPanelCollapsed, setActiveTab]);

  const isResizing = useRef(false);
  const startX = useRef(0);
  const startWidth = useRef(DEFAULT_SIDEBAR_WIDTH);
  const currentWidthRef = useRef(DEFAULT_SIDEBAR_WIDTH);
  const isResizingRight = useRef(false);
  const startXRight = useRef(0);
  const startWidthRight = useRef(DEFAULT_RIGHT_WIDTH);
  const currentWidthRefRight = useRef(DEFAULT_RIGHT_WIDTH);
  const isResizingFile = useRef(false);
  const startXFile = useRef(0);
  const startWidthFile = useRef(DEFAULT_FILE_WIDTH);
  const currentWidthRefFile = useRef(DEFAULT_FILE_WIDTH);

  // ADR-052: one-shot health probe on mount - NOT a poll.
  // SplashScreen is the startup orchestrator: it calls `checkHealth()`
  // inside `finish()` before invoking `onReady()`, so by the time this
  // effect runs `status` should already be `connected`. This single probe
  // is only a safety net for edge cases (remote-gateway mode, an older
  // Rust binary that skipped the probe, store state reset) and cannot
  // flicker the indicator - the title-bar gateway button only changes
  // shape for `error`-class steady-state outages.
  useEffect(() => {
    checkHealth();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  // ADR-052: AppLayout is mounted only AFTER SplashScreen confirms the
  // Gateway has booted, so by the time this effect runs `status` should be
  // `connected`. If we ever see `error` here, it is a *real* post-boot drop.
  // Any other transient state (`connecting` / `disconnected`) is owned by
  // SplashScreen and must not surface in the bottom bar - that bar is
  // dedicated to post-startup liveness signals.
  useEffect(() => {
    if (gatewayStatus === "error") {
      setStatus("Gateway connection failed", "error", "gateway");
    } else {
      clearStatus();
    }
  }, [gatewayStatus, setStatus, clearStatus]);

  // ── Gateway liveness: drop/rise from the single-authority verdict ──
  // `gatewayAlive` is maintained by `gatewayStore`: MQTT CONNACK is the
  // liveness authority (the broker lives inside the Gateway process),
  // and the HTTP probe is a subordinate death classifier that runs only
  // while MQTT is down. When the verdict lands on `dead`, nothing else
  // will arrive to refresh the UI — the broker died with the Gateway —
  // so the drop edge mirrors the natural `agent_status offline` MQTT
  // flow (agents marked offline, their sessions released, the node
  // snapshot marked offline). The rise edge re-pulls the agent list +
  // node topology and re-probes services. `unknown` never fires an
  // edge, so a link flap that was never classified leaves the UI
  // untouched.
  //
  // The edge-detection logic lives in `gatewayTransition.ts` so it is
  // unit-testable without rendering React. The effect here is a one-liner
  // that wires store actions into the pure helper.
  const prevGatewayAliveRef = useRef<GatewayAlive | null>(null);
  useEffect(() => {
    const prev = prevGatewayAliveRef.current;
    prevGatewayAliveRef.current = gatewayAlive;
    applyGatewayTransition(prev, gatewayAlive, {
      getAgentIds: () => Object.keys(useAgentStore.getState().agents),
      setAgentOffline: (id) => useAgentStore.getState().updateAgentLiveness(id, false),
      clearAgentSessions: (id) => useChatStore.getState().clearAgentSessions(id),
      refreshServices: () => {
        void useServicesStore.getState().diagnose();
      },
      markNodesOffline: () => useAgentStore.getState().markNodesOffline(),
      fetchAgents: () => useAgentStore.getState().fetchAgents(),
      fetchNodes: () => useAgentStore.getState().fetchNodes(),
    });
  }, [gatewayAlive]);

  // ── MQTT edge → liveness wiring ────────────────────────────────────
  // CONNACK (rise): the broker lives inside the Gateway process, so the
  // connack is sufficient proof of liveness. Cancel the in-flight
  // classifier probe FIRST — its question is moot, and a superseded
  // probe must never write a stale verdict — then stop the watch, mark
  // the verdict `alive`, and take a fresh probe for the health payload.
  // A drop (MQTT down) only starts the death classifier: MQTT down
  // alone does NOT mean the Gateway died. See `onMqttConnectionEdge`.
  const prevMqttConnectedRef = useRef<boolean | null>(null);
  useEffect(() => {
    const prev = prevMqttConnectedRef.current;
    prevMqttConnectedRef.current = mqttConnected;
    onMqttConnectionEdge(prev, mqttConnected, {
      onRise: () => {
        cancelHealthProbe("mqtt-connected");
        stopGatewayDeathWatch();
        markGatewayAlive("mqtt-connack");
        void checkHealth();
      },
      onDrop: () => {
        startGatewayDeathWatch();
      },
    });
  }, [mqttConnected, checkHealth]);

  // ADR-036: surface MQTT connection liveness in the status bar.
  //
  // The Rust eventloop is the source-of-truth (it observes CONNACK /
  // DISCONNECT on `rumqttc`'s eventloop and emits `mqtt-status` events
  // to the frontend).  We do NOT trigger any reconnect from here —
  // reconnect is owned by `rumqttc`'s built-in retry; we only reflect
  // the state so the user sees when input has gone disabled.
  //
  // Status priority: a Gateway-level warning/error takes precedence over
  // a transient MQTT disconnect, so we only update if Gateway is healthy.
  //
  // **Distinguishing "never connected" from "lost connection"**: the
  // store starts with `lastMqttError === null`, so we use it as the
  // "have we ever observed a disconnect?" flag.  Without this guard the
  // banner flashes for ~1s on every cold start (between Rust spawn and
  // the first mqtt-status event reaching the frontend).
  useEffect(() => {
    if (gatewayStatus !== "connected") return;
    // Auto-sleep was retired in Sept 2026 — every MQTT disconnect is a
    // real connection problem, not the expected aftermath of an
    // auto-slept Runtime. Surface the warning unconditionally.
    if (!mqttConnected && lastMqttError !== null) {
      const reason = lastMqttError ? `: ${lastMqttError}` : "";
      setStatus(t("statusBar.mqttDisconnected", { reason }), "warning", "mqtt");
    } else if (mqttConnected) {
      // MQTT just reconnected - clear the MQTT-disconnected status
      // by source rather than by fragile text matching on the
      // displayed string (which would break if translations change).
      clearStatus("mqtt");
    }
  }, [mqttConnected, lastMqttError, gatewayStatus, setStatus, clearStatus, t]);

  // Detect wake from sleep via visibility change and reconnect.
  //
  // ADR-036 originally declared "we do NOT touch the MQTT client here"
  // and relied entirely on the Rust `rumqttc` eventloop + 90 s watchdog
  // to recover from OS sleep/wake. Empirically that leaves the user
  // staring at a "Realtime connection lost, retrying..." banner for up
  // to 90 s after every wake event - and on some OSes the kernel never
  // detects the broken TCP connection at all (half-dead socket), so
  // the watchdog is the *only* recovery path. The fix:
  //
  //   1. Frontend proactively invokes `force_reconnect_mqtt` on wake
  //      whenever the store says we are disconnected. The Rust side
  //      drops the EventLoop + AsyncClient and creates a fresh pair,
  //      which immediately fails-or-succeeds against the (possibly
  //      just-restored) TCP socket - no need to wait for the watchdog.
  //   2. Debounced: a flurry of `visibilitychange` events (e.g. user
  //      switches apps then back rapidly) only triggers ONE reconnect.
  //   3. Falls back to `checkHealth()` if the Tauri command is missing
  //      or transiently rejects (e.g. during shutdown).
  useEffect(() => {
    let pending = false;
    let debounceHandle: ReturnType<typeof setTimeout> | null = null;

    const triggerForceReconnect = () => {
      if (pending) return;
      pending = true;
      // 500 ms window: collect a burst of visibilitychange events
      // into a single reconnect attempt.
      debounceHandle = setTimeout(() => {
        debounceHandle = null;
        const mqttConnected = useChatStore.getState().mqttConnected;
        if (mqttConnected) {
          log.debug("[AppLayout] visibility wake - already connected, skipping force_reconnect");
          pending = false;
          return;
        }
        log.debug("[AppLayout] visibility wake - forcing MQTT reconnect");
        invoke("force_reconnect_mqtt")
          .then(() => {
            log.debug("[AppLayout] force_reconnect_mqtt ok");
          })
          .catch((e) => {
            // Old binary without the command, or MQTT not yet
            // connected at all - fall back to health check so the
            // user still sees current Gateway status.
            log.warn("[AppLayout] force_reconnect_mqtt failed, falling back to checkHealth:", e);
            checkHealth();
          })
          .finally(() => {
            pending = false;
          });
      }, 500);
    };

    const handleVisibility = () => {
      if (document.visibilityState !== "visible") return;
      const a = useAgentStore.getState();
      const c = useChatStore.getState();
      const sid = a.selectedAgentId ?? "";
      log.debug("[AppLayout] Page visible after sleep/lock", {
        selectedAgentId: a.selectedAgentId,
        activeSessionId: c.agentStates[sid]?.activeSessionId ?? null,
        openSessionIds: c.agentStates[sid]?.openSessionIds ?? [],
        knownAgents: Object.keys(a.agents),
        sessionsForSelected: a.selectedAgentId ? (a.agents[a.selectedAgentId]?.sessions ?? []).map((s) => s.session_id) : null,
      });
      // Always poke Gateway health so the user sees current status,
      // even when MQTT is still connected (sanity check on wake).
      checkHealth();
      // Only force-reconnect MQTT when we believe it is down.
      // Otherwise the active connection would be torn down for no
      // reason, causing a brief UI flash.
      if (!useChatStore.getState().mqttConnected) {
        triggerForceReconnect();
      }
    };
    document.addEventListener("visibilitychange", handleVisibility);
    return () => {
      document.removeEventListener("visibilitychange", handleVisibility);
      if (debounceHandle) {
        clearTimeout(debounceHandle);
        debounceHandle = null;
      }
    };
  }, [checkHealth]);

  // Scale file panel proportionally when window size changes significantly (maximize/restore).
  // Sidebar and right panel keep their absolute widths; only session & file panels scale.
  // Small manual edge-drags (<5%) are ignored to avoid jitter.
  const NAV_WIDTH = 48;
  const prevAvailableWidthRef = useRef(window.innerWidth - sidebarWidth - (rightPanelCollapsed ? 0 : rightWidth) - NAV_WIDTH);
  useEffect(() => {
    const handleWindowResize = () => {
      // Don't scale during manual panel resize
      if (isResizingFile.current) return;

      const newWindowWidth = window.innerWidth;
      const constantWidths = sidebarWidthRef.current + (rightPanelCollapsedRef.current ? 0 : rightWidthRef.current) + NAV_WIDTH;
      const newAvailable = newWindowWidth - constantWidths;
      const prevAvailable = prevAvailableWidthRef.current;

      // Guard against zero or negative available space
      if (prevAvailable <= 0 || newAvailable <= 0) return;

      const ratio = newAvailable / prevAvailable;

      // Only scale when available space changes significantly (>5%)
      if (Math.abs(ratio - 1) < 0.05) return;

      prevAvailableWidthRef.current = newAvailable;

      // Scale the file panel by the available-space ratio; ChatPanel (flex-1) gets the rest.
      // This preserves the same proportion of file vs session within the available space.
      const hasFiles = useFileEditorStore.getState().openFiles.length > 0;
      if (hasFiles) {
        const newFile = Math.min(Math.max(Math.round(fileWidthValueRef.current * ratio), MIN_FILE_WIDTH), MAX_FILE_WIDTH);
        setFileWidth(newFile);
        localStorage.setItem(FILE_WIDTH_KEY, String(newFile));
      }
    };

    window.addEventListener("resize", handleWindowResize);
    return () => window.removeEventListener("resize", handleWindowResize);
  }, []);

  // Absorb chat-view overflow. ChatPanel (`flex-1`) takes the first wave of
  // pressure by itself; once it is pinned at MIN_CHAT_WIDTH every pane in the
  // row is `shrink-0`, so opening the right panel used to add `rightWidth`
  // pixels that nothing gave back and the row overflowed the window instead of
  // re-flowing. `fitPanesToViewport` decides who pays: file pane first, then
  // the right pane, never the agent list (see panelClamp.ts for why width is
  // the wrong thing to rank by).
  //
  // Not written to localStorage on purpose — the stored value stays whatever
  // the user dragged to, so a restart with the right panel collapsed gets the
  // maxed file pane back. The clamp is idempotent, so each state write below
  // converges rather than oscillating.
  useEffect(() => {
    if (!hasOpenFiles || !windowWidth) return;
    const fit = fitPanesToViewport({ windowWidth, sidebarWidth, fileWidth, rightWidth: rightPaneWidth });
    if (fit.fileWidth !== fileWidth) setFileWidth(fit.fileWidth);
    if (fit.rightWidth !== rightPaneWidth) setRightWidth(fit.rightWidth);
  }, [windowWidth, sidebarWidth, fileWidth, rightPaneWidth, hasOpenFiles]);

  const toggleRightPanel = useCallback(() => {
    setRightPanelCollapsed((prev) => !prev);
  }, []);

  // Navigate to settings with profile tab when avatar is clicked
  const handleAvatarClick = useCallback(() => {
    setSettingsInitialTab("profile");
    setCurrentView("settings");
  }, []);

  // Navigate via nav bar — reset settings tab to default
  const handleViewChange = useCallback((view: NavView) => {
    if (view === "settings") {
      setSettingsInitialTab("profile");
    }
    setCurrentView(view);
  }, []);

  // Views opened from outside the nav bar (e.g. the sidebar UserList starting
  // a user conversation, ADR-076 §决策 8). `currentView` is local state, so
  // the request travels through the layout store's seq contract and is
  // consumed exactly once — same pattern as the workspace-search focus
  // request in `WorkspaceExplorer`.
  const navViewRequestSeq = useLayoutStore((s) => s.navViewRequestSeq);
  const consumedNavViewRequestRef = useRef(0);
  useEffect(() => {
    if (navViewRequestSeq <= consumedNavViewRequestRef.current) return;
    consumedNavViewRequestRef.current = navViewRequestSeq;
    const target = useLayoutStore.getState().navViewRequest;
    if (target) setCurrentView(target);
  }, [navViewRequestSeq]);

  // Sidebar resize handlers
  const handleMouseMove = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizing.current) return;
    const delta = e.clientX - startX.current;
    const rawWidth = startWidth.current + delta;

    if (currentWidthRef.current === AVATAR_SIDEBAR_WIDTH) {
      // In collapsed state — only expand when dragged back past MIN_SIDEBAR_WIDTH
      if (rawWidth >= MIN_SIDEBAR_WIDTH) {
        const newWidth = Math.min(rawWidth, MAX_SIDEBAR_WIDTH);
        currentWidthRef.current = newWidth;
        setSidebarWidth(newWidth);
      }
      return;
    }

    if (rawWidth < MIN_SIDEBAR_WIDTH) {
      // Crossed below minimum — collapse to avatar width
      currentWidthRef.current = AVATAR_SIDEBAR_WIDTH;
      setSidebarWidth(AVATAR_SIDEBAR_WIDTH);
    } else if (rawWidth > MAX_SIDEBAR_WIDTH) {
      currentWidthRef.current = MAX_SIDEBAR_WIDTH;
      setSidebarWidth(MAX_SIDEBAR_WIDTH);
    } else {
      currentWidthRef.current = rawWidth;
      setSidebarWidth(rawWidth);
    }
  }, []);

  const handleMouseUp = useCallback(() => {
    if (!isResizing.current) return;
    isResizing.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMove);
    document.removeEventListener("mouseup", handleMouseUp);
    localStorage.setItem(SIDEBAR_WIDTH_KEY, String(currentWidthRef.current));
  }, [handleMouseMove]);

  const handleMouseDown = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizing.current = true;
    startX.current = e.clientX;
    startWidth.current = sidebarWidth;
    currentWidthRef.current = sidebarWidth;
    document.addEventListener("mousemove", handleMouseMove);
    document.addEventListener("mouseup", handleMouseUp);
  }, [handleMouseMove, handleMouseUp, sidebarWidth]);

  // Right panel resize handlers
  const handleMouseMoveRight = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizingRight.current) return;
    const delta = e.clientX - startXRight.current;
    const newWidth = Math.min(Math.max(startWidthRight.current - delta, MIN_RIGHT_WIDTH), MAX_RIGHT_WIDTH);
    currentWidthRefRight.current = newWidth;
    setRightWidth(newWidth);
  }, []);

  const handleMouseUpRight = useCallback(() => {
    if (!isResizingRight.current) return;
    isResizingRight.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMoveRight);
    document.removeEventListener("mouseup", handleMouseUpRight);
    localStorage.setItem(RIGHT_WIDTH_KEY, String(currentWidthRefRight.current));
  }, [handleMouseMoveRight]);

  const handleMouseDownRight = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizingRight.current = true;
    startXRight.current = e.clientX;
    startWidthRight.current = rightWidth;
    currentWidthRefRight.current = rightWidth;
    document.addEventListener("mousemove", handleMouseMoveRight);
    document.addEventListener("mouseup", handleMouseUpRight);
  }, [handleMouseMoveRight, handleMouseUpRight, rightWidth]);

  // File panel resize handlers — dynamic max width to keep ChatPanel visible
  const maxFileWidthRef = useRef(MAX_FILE_WIDTH);

  const handleMouseMoveFile = useCallback((e: MouseEvent) => {
    e.preventDefault();
    if (!isResizingFile.current) return;
    const delta = e.clientX - startXFile.current;
    const newWidth = Math.min(Math.max(startWidthFile.current - delta, MIN_FILE_WIDTH), maxFileWidthRef.current);
    currentWidthRefFile.current = newWidth;
    setFileWidth(newWidth);
  }, []);

  const handleMouseUpFile = useCallback(() => {
    if (!isResizingFile.current) return;
    isResizingFile.current = false;
    document.body.style.userSelect = '';
    document.removeEventListener("mousemove", handleMouseMoveFile);
    document.removeEventListener("mouseup", handleMouseUpFile);
    localStorage.setItem(FILE_WIDTH_KEY, String(currentWidthRefFile.current));
  }, [handleMouseMoveFile]);

  const handleMouseDownFile = useCallback((e: React.MouseEvent) => {
    e.preventDefault();
    document.body.style.userSelect = 'none';
    isResizingFile.current = true;
    startXFile.current = e.clientX;
    startWidthFile.current = fileWidth;
    currentWidthRefFile.current = fileWidth;
    // Ceiling for the drag: everything left after the rigid panes and the
    // ChatPanel minimum. Uses the same CHROME_WIDTH as the overflow clamp
    // below — it used to subtract only the 48px nav bar, which let a drag
    // grow the file pane past the 40px right rail and overflow the row.
    const dynamicMax = Math.max(windowWidth - CHROME_WIDTH - sidebarWidth - rightPaneWidth - MIN_CHAT_WIDTH, MIN_FILE_WIDTH);
    maxFileWidthRef.current = Math.min(MAX_FILE_WIDTH, dynamicMax);
    document.addEventListener("mousemove", handleMouseMoveFile);
    document.addEventListener("mouseup", handleMouseUpFile);
  }, [handleMouseMoveFile, handleMouseUpFile, fileWidth, sidebarWidth, rightPaneWidth, windowWidth]);

  // ── ADR-081: global search shortcut (Ctrl/Cmd+Shift+F) ──────────────
  // Window-level handler so the dialog opens regardless of focus; while the
  // Monaco editor is focused its own addAction intercepts first (same store).
  useEffect(() => {
    const onKeyDown = (e: KeyboardEvent) => {
      if (!isGlobalSearchShortcut(e)) return;
      e.preventDefault();
      useSearchStore.getState().toggleDialog();
    };
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, []);

  return (
    <div className="flex h-full w-full flex-col" style={{ backgroundColor: glassBg } as React.CSSProperties}>
      {/* Custom title bar — on macOS, sits under the native traffic lights
          (titleBarStyle:"Overlay"). On Windows/Linux, decorations are
          disabled in Rust setup() so this is the only title bar. */}
      <TitleBar />

      {/* ADR-052: the Gateway outage indicator only renders for
          *steady-state* drops. AppLayout is gated by `gatewayReady` in
          App.tsx, so by the time we get here SplashScreen has already
          pushed `status` to `connected`. Showing it for any state other
          than `error` would re-introduce the pre-SplashScreen-era flicker
          where the indicator appeared during the boot window despite
          SplashScreen orchestrating the startup correctly.

          It renders inside <TitleBar /> as a status chip — a drop after a
          sleep/wake network switch persists until the user acts, so the
          affordance must be permanent, but a full-width strip across the
          top of every view out-shouted the content it was reporting. */}

      {/* Main content area */}
      <div className="flex flex-1 overflow-hidden">
        {/* Navigation bar — 48px */}
        <NavBar currentView={currentView} onViewChange={handleViewChange} onAvatarClick={handleAvatarClick} />

        {/* Content area based on current view */}
        {currentView === "chat" && (
          // Chat-view capsule container — transparent so the OS-native
          // vibrancy layer (set by `set_window_effect` in src-tauri) and
          // the CSS-layer glass tint (this file's `glassBg`) show through.
          // The four capsule panels (AgentList / ChatPanel / FileEditorPanel
          // / RightPanel) carry their own borders, which is what now
          // outlines them as floating surfaces. The non-chat view branches
          // below (settings/harness/pm/docs/extensions) keep their wrappers
          // transparent for the same reason.
          <div className="flex flex-1 overflow-hidden rounded-xl">
            {/* Agent list — resizable */}
            <AgentList width={sidebarWidth} />

            {/* Resize handle */}
            <div
              className="group relative w-1 shrink-0 cursor-col-resize select-none"
              onMouseDown={handleMouseDown}
              role="separator"
              aria-label={t("appLayout.ariaLabelResizeSidebar")}
            >
              {/* Visible divider line — removed, use glass bg as separator */}
              <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors rounded-full" />
            </div>

            {/* Middle panel — agent session takes priority; an open inbox
                thread falls back to the user chat composer. Empty state is
                handled by the panel components themselves. */}
            {selectedAgentId ? <ChatPanel /> : activePeerId ? <InboxPanel /> : <EmptyChatPane />}

            {/* File editor panel — shown when files are open */}
            {hasOpenFiles && (
              <>
                {/* Resize handle between chat and file editor */}
                <div
                  className="group relative w-1 shrink-0 cursor-col-resize select-none"
                  onMouseDown={handleMouseDownFile}
                  role="separator"
                  aria-label={t("appLayout.ariaLabelResizeFileEditor")}
                >
                  <div className="absolute inset-y-0 left-0 w-1 group-hover:bg-[var(--color-accent)]/30 group-active:bg-[var(--color-accent)]/60 transition-colors rounded-full" />
                </div>
                <ChunkLoadBoundary
                  key={editorLoadAttempt}
                  fallback={
                    <div className="flex h-full w-full flex-col items-center justify-center gap-3 px-4 text-center">
                      <AlertTriangle className="h-6 w-6 text-amber-500" />
                      <p className="text-sm text-text-tertiary ">
                        {t("errorBoundary.title")}
                      </p>
                      {/* Retry re-imports the chunk (works when the fetch failed
                          once, e.g. a blip). Reload is the only way out when the
                          page itself is stale — e.g. a tray window that outlived
                          its dev server, whose module URLs will never resolve
                          again. The full-screen ErrorBoundary offers the same
                          pair. */}
                      <div className="flex items-center gap-2">
                        <button
                          type="button"
                          onClick={() => setEditorLoadAttempt((n) => n + 1)}
                          className="flex items-center gap-2 rounded-md bg-zinc-200 px-3 py-1.5 text-sm font-medium text-text-secondary hover:bg-zinc-300 dark:bg-zinc-700  dark:hover:bg-zinc-600"
                        >
                          <RefreshCw className="h-4 w-4" />
                          {t("errorBoundary.retry")}
                        </button>
                        <button
                          type="button"
                          onClick={() => window.location.reload()}
                          className="flex items-center gap-2 rounded-md px-3 py-1.5 text-sm font-medium text-white"
                          style={{ backgroundColor: "var(--color-accent)" }}
                          onMouseEnter={(e) => { e.currentTarget.style.filter = "brightness(0.85)"; }}
                          onMouseLeave={(e) => { e.currentTarget.style.filter = ""; }}
                        >
                          {t("errorBoundary.refreshPage")}
                        </button>
                      </div>
                    </div>
                  }
                >
                  {/*
                   * Suspense fallback mirrors the FileEditorPanel shell
                   * (CAPSULE_PANE_CN + shrink-0 + bg-right-panel) so the
                   * lazy chunk paints into an opaque panel-shaped slot
                   * instead of an empty <div>. Without this the file pane
                   * flashed transparent glass for one frame the first time
                   * a file was opened in a session — the same flicker bug
                   * fixed on the chat side via CHAT_PANEL_SHELL_CN.
                   */}
                  <Suspense
                    fallback={
                      <div
                        aria-hidden="true"
                        className={cn(CAPSULE_PANE_CN, "shrink-0 bg-right-panel")}
                        style={{ width: fileWidth }}
                      />
                    }
                  >
                    <FileEditorPanel width={fileWidth} />
                  </Suspense>
                </ChunkLoadBoundary>
              </>
            )}

            {/* Right panel — unified tabs, collapsible, resizable. Only
                meaningful for an agent session: every tab (workspace /
                status / memory) reads the selected agent's Runtime, so with
                no agent there is nothing truthful to show. It used to fall
                back to an outlined placeholder capsule of the same width to
                keep the four-capsule layout intact, but an empty frame only
                widened the gap to the 40px right rail without adding
                information — worse, the placeholder was a separate element
                from RightPanel and so missed that panel's `ml-1` gutter,
                making the chat pane and the frame touch. No panel, no
                gutter to get wrong. */}
            {!rightPanelCollapsed && selectedAgentId && (
              <RightPanel width={rightWidth} onCollapse={toggleRightPanel} isDebugMode={isDebugMode} onResizeStart={handleMouseDownRight} activeTab={activeTab} onTabChange={setActiveTab} />
            )}
          </div>
        )}

        {/* Right rail — 40px column. Renders the agent-config nav buttons
            when an agent is selected; otherwise stays as an empty
            placeholder of the same width and padding so the window chrome
            remains symmetric across views (mirrors the non-chat-view
            placeholder below). Glass background bleeds through both
            branches (no explicit bg). */}
        {currentView === "chat" ? (
          selectedAgentId ? (
            <RightNavBar
              activeTab={activeTab}
              onTabChange={(tab) => {
                if (!rightPanelCollapsed && tab === activeTab) {
                  setRightPanelCollapsed(true);
                } else {
                  setRightPanelCollapsed(false);
                  setActiveTab(tab);
                }
              }}
              agentRunning={selectedAgent?.alive ?? false}
              collapsed={rightPanelCollapsed}
            />
          ) : (
            // User-chat / EmptyChatPane: no RightNavBar items yet (the
            // agent-config tabs are gated on a selected agent), but keep
            // the 40px column reserved so the chat pane doesn't bleed to
            // the window's right edge and break visual symmetry with the
            // other views. Matches the non-chat-view placeholder below.
            <aside className="w-10 shrink-0 py-2 dark:border-zinc-800" aria-hidden="true" />
          )
        ) : null}

        {currentView === "settings" && (
          <div className="flex flex-1 overflow-hidden rounded-xl">
            <SettingsPage initialTab={settingsInitialTab} />
          </div>
        )}

        {currentView === "harness" && (
          <div className="flex flex-1 overflow-hidden rounded-xl">
            <HarnessPage />
          </div>
        )}

        {/* Every non-chat view shares the chat view's capsule layout
            (rounded panes on the window's vibrancy, see the chat branch
            above), so the wrapper is transparent — each view paints its own
            panes via CAPSULE_PANE_CN. They used to carry `bg-page-bg` here,
            which walled the whole view off from the desktop and read as a
            different app. settings / harness now join the same two-column
            master-detail shape (SectionPane: list capsule + detail capsule)
            as pm / docs / extensions, instead of a row of inline tabs on a
            solid page. */}
        {currentView === "projects" && (
          <div className="flex flex-1 overflow-hidden rounded-xl">
            <ProjectsView />
          </div>
        )}

        {currentView === "docs" && (
          <div className="flex flex-1 overflow-hidden rounded-xl">
            <DocsView />
          </div>
        )}

        {currentView === "extensions" && (
          <div className="flex flex-1 overflow-hidden rounded-xl">
            <ExtensionsView />
          </div>
        )}

        {/* Right rail placeholder for non-chat views — keeps the 40px right
            column reserved so window chrome stays symmetric when switching
            between nav targets. Positioned last so it always sits at the
            right edge, regardless of which central panel is active. */}
        {currentView !== "chat" && (
          <aside className="w-10 shrink-0 py-2 dark:border-zinc-800" aria-hidden="true" />
        )}
      </div>

      {/* Bottom status bar */}
      {/* Per-key:value pill style: opaque backdrop so the text stays readable when window opacity < 1 */}
      {/* `relative` so the file-status cluster can anchor absolutely to */}
      {/* the file editor panel's left/right edges via PR-1's filePanelBounds. */}
      <div className="relative flex h-6 shrink-0 items-center gap-2 pl-14 pr-3 text-11 select-none ">
        {statusVisible && (
          <Tooltip
            content={statusMsg}
            variant="plain"
            position="top"
            delayMs={200}
            maxWidth="60vw"
          >
            <button
              type="button"
              onClick={handleCopyStatusMsg}
              aria-label={t("common.ariaLabelCopyError")}
              title={t("common.copy")}
              className={cn(
                // `max-w-[min(60%,32rem)]` caps the pill so a long
                // error never squeezes the agent/context pills off
                // screen. `truncate` (already on the inner span) only
                // engages once the button has a finite width.
                "inline-flex items-center gap-1 rounded-md px-2 py-0.5 max-w-[min(60%,32rem)] cursor-pointer transition-colors",
                "focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-[var(--color-accent)]/40",
                statusType === "error" &&
                  "text-red-600 dark:text-red-400 bg-red-100 dark:bg-red-950/70 border-red-300/70 dark:border-red-800/70 hover:bg-red-200/80 dark:hover:bg-red-900/70",
                statusType === "warning" &&
                  "text-amber-700 dark:text-amber-300 bg-amber-100 dark:bg-amber-950/70 border-amber-300/70 dark:border-amber-800/70 hover:bg-amber-200/80 dark:hover:bg-amber-900/70",
                statusType === "info" &&
                  "text-text-tertiary  bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60 hover:bg-zinc-200/80 dark:hover:bg-zinc-700/75",
              )}
            >
              {statusCopied ? (
                <>
                  <Check className="h-3 w-3 shrink-0" aria-hidden="true" />
                  <span className="truncate">{t("common.copied")}</span>
                </>
              ) : (
                <span className="truncate">{statusMsg}</span>
              )}
            </button>
          </Tooltip>
        )}
        {(rightPanelCollapsed || activeTab !== "status") && selectedAgent?.alive && agentDisplayName && (
          <span className="flex items-center gap-2 truncate">
            <span className="flex items-center gap-1 pl-1 pr-4 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
              <Bot className="h-3 w-3 text-text-tertiary " aria-hidden="true" />
              <span className="text-text-tertiary ">{t("statusBar.agent")}: </span>
              <span className="font-medium text-text-tertiary ">{agentDisplayName}</span>
            </span>
            {contextUsage && (
              <span className="flex items-center gap-1 px-2 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
                <Cpu className="h-3 w-3 text-text-tertiary " aria-hidden="true" />
                <span className="text-text-tertiary ">{t("statusBar.context")}: </span>
                <span
                  className="tabular-nums font-medium text-text-tertiary "
                  style={{
                    color:
                      contextUsage.usage_percent >= 90
                        ? "var(--color-accent)"
                        : undefined,
                  }}
                >
                  {formatPercent(contextUsage.usage_percent)}%
                </span>
                <span className="text-text-tertiary "> | </span>
                <span className="tabular-nums font-medium text-text-tertiary ">
                  {formatTokenCount(contextUsage.total_tokens)}/{formatTokenCount(contextUsage.context_window)}
                </span>
              </span>
            )}
            {/* ADR-066: cache hit rate — deliberately terse (`90% cached`)
                because the status bar has limited horizontal space.  Shows
                the session-lifetime (cumulative) rate.  Shown whenever the
                Runtime reports any cumulative cache accounting; the ratio
                falls back to a dash when it isn't computable yet. */}
            {hasCacheData(contextUsage, "cumulative") && (
              <span className="flex items-center gap-1 px-2 py-px rounded-md bg-zinc-100/80 dark:bg-zinc-800/75 border border-zinc-200/50 dark:border-zinc-700/60">
                <span className="tabular-nums font-medium text-text-tertiary ">
                  {cacheHitRateLabel ?? "\u2014"}
                </span>
                <span className="text-text-tertiary ">{t("statusBar.cached")}</span>
              </span>
            )}
          </span>
        )}
        <MqttDebugControls />

        {/* File-status cluster (PR-2). Absolutely positioned inside this */}
        {/* bar so it visually floats over the file editor column without */}
        {/* forcing the other pills to reflow. Hidden when no file is open */}
        {/* (the panel is unmounted, so bounds stay at mounted:false) — */}
        {/* keeps the global bar untouched for the dominant no-file state. */}
        {filePanelBounds.mounted && (
          <div
            style={{
              position: "absolute",
              display: "flex",
              alignItems: "center",
              left: filePanelBounds.left,
              // `right` is measured from window right edge; CSS clamps
              // negative values to 0, so a window narrower than the
              // panel's last known right edge degrades to full bleed
              // until ResizeObserver catches up.
              right: Math.max(0, windowWidth - filePanelBounds.right),
            }}
          >
            <FileStatusCluster
              activeFile={clusterActiveFile}
              cursor={editorCursor}
              selectedCount={editorSelectedCount}
              lspEnabled={editorLspEnabled}
              lspLanguage={editorLspLanguage}
              lspStatus={editorLspStatus}
              lspStatusMessage={editorLspStatusMessage}
            />
          </div>
        )}
      </div>

      {/* ADR-081: app-wide global search dialog (Ctrl+Shift+F). Fixed
          overlay; mounted regardless of current view. */}
      <GlobalSearchDialog onNavigate={(view) => setCurrentView(view)} />

    </div>
  );
}

function formatTokenCount(n: number | null | undefined): string {
  if (typeof n !== "number" || !Number.isFinite(n)) return "—";
  if (n >= 1_000_000) return `${(n / 1_000_000).toFixed(1)}M`;
  if (n >= 1_000) return `${(n / 1_000).toFixed(1)}K`;
  return n.toString();
}
