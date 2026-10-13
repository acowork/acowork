import { create } from "zustand";
import { invoke } from "@tauri-apps/api/core";
import type { Theme, GatewayMode } from "../lib/types";
import {
  DEFAULT_GATEWAY_URL,
  DEFAULT_THEME,
  DEFAULT_FONT_SIZE,
  DEFAULT_LOG_LEVEL,
  DEFAULT_CONTENT_WIDTH,
  DEFAULT_OPACITY,
  DEFAULT_ACCENT_COLOR,
  DEFAULT_GATEWAY_MODE,
  DEFAULT_LOG_FILE_SIZE_MB,
  DEFAULT_LOG_FILE_COUNT,
  DEFAULT_FRONTEND_LOG_LEVEL,
} from "../lib/defaults";
import {
  setLevel as setLoggerLevel,
  type LogLevel,
} from "../lib/logger";
import { log } from "../lib/logger";
import i18n from "../i18n";

/** Check for the default-port Node before committing a Gateway mode switch. */
export async function requestGatewayModeChange(mode: GatewayMode): Promise<void> {
  if (mode === useSettingsStore.getState().gatewayMode) return;
  try {
    const nodePids = await invoke<number[]>("inspect_local_node_process");
    if (nodePids.length > 0) {
      const confirmed = window.confirm(i18n.t("onboarding.gateway.nodeSwitchConfirm"));
      if (!confirmed) return;
      await invoke("stop_local_node_process");
    }
    useSettingsStore.getState().setGatewayMode(mode);
  } catch (error) {
    log.warn("Failed to handle local Node during Gateway mode switch:", error);
    window.alert(`${i18n.t("onboarding.gateway.nodeSwitchFailed")}\n${String(error)}`);
  }
}

import { useAuthStore } from "./authStore";
import {
  DEFAULT_ACCENT_PRESET,
  getAccentPresetByHex,
  type AccentPreset,
} from "../lib/accentPresets";

/**
 * Push the current gateway config to the Rust backend so that:
 *   - All Tauri HTTP commands use the correct base URL (previously they
 *     were hardcoded to 127.0.0.1:19876 in Rust)
 *   - The Rust side knows whether to skip spawning a local Gateway on
 *     the next boot (remote mode)
 *   - An address change also rebuilds the MQTT connection: the Rust
 *     `connect_mqtt` command detects that the configured broker differs
 *     from the one the live client was created for and reconnects — no
 *     app restart needed
 *
 * Best-effort: errors are logged but never thrown, because settings
 * persistence must not be blocked by transient Tauri command failures
 * (e.g. during page reload while the Rust side is still booting).
 */
async function pushGatewayConfigToRust(mode: GatewayMode, url: string): Promise<void> {
  try {
    await invoke("set_gateway_config", {
      config: { mode, url },
    });
    // The MQTT client derives its broker host/port from the Gateway URL
    // at creation time. Re-run `connect_mqtt` here so saving a new
    // address — SplashScreen timeout retry or Settings — tears down a
    // stale connection and rebuilds it against the configured broker.
    // No-op when the endpoint is unchanged.
    try {
      await invoke("connect_mqtt");
    } catch {
      // Rust-side MQTT may not be booted yet (e.g. page reload before
      // the SplashScreen boot flow runs) — the boot path covers it.
    }
  } catch (err) {
    log.warn("Failed to push gateway config to Rust:", err);
  }
}

const STORAGE_KEY_THEME = "acowork-theme";
const STORAGE_KEY_FONT_SIZE = "acowork-font-size";
const STORAGE_KEY_LOG_LEVEL = "acowork-log-level";
const STORAGE_KEY_CONTENT_WIDTH = "acowork-content-width";
const STORAGE_KEY_OPACITY = "acowork-opacity";
const STORAGE_KEY_ACCENT_COLOR = "acowork-accent-color";
const STORAGE_KEY_GATEWAY_URL = "acowork-gateway-url";
const STORAGE_KEY_GATEWAY_URL_HISTORY = "acowork-gateway-url-history";
const STORAGE_KEY_GATEWAY_MODE = "acowork-gateway-mode";
/** Remote-mode auto-start of the local Node on Desktop launch (ADR-087 follow-up). */
const STORAGE_KEY_AUTO_START_LOCAL_NODE = "acowork-auto-start-local-node";

/** Max retained gateway URLs in localStorage. 8 covers typical LAN/office/home/relay switches without leaking storage. */
const GATEWAY_URL_HISTORY_MAX = 8;
const STORAGE_KEY_LOG_FILE_SIZE = "acowork-log-file-size";
const STORAGE_KEY_LOG_FILE_COUNT = "acowork-log-file-count";
const STORAGE_KEY_FRONTEND_LOG_LEVEL = "acowork-frontend-log-level";

/** Read current OS theme preference from matchMedia */
function readOsTheme(): "light" | "dark" {
  if (typeof window === "undefined" || !window.matchMedia) return "light";
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** Apply theme to DOM by toggling .dark class on <html>.
 * For "system" mode, `osTheme` must be passed so the correct value
 * is applied even when the OS theme changed since the last call. */
function applyTheme(theme: Theme, osTheme: "light" | "dark") {
  const effective = theme === "system" ? osTheme : theme;
  document.documentElement.classList.toggle("dark", effective === "dark");
}

/** Apply fontSize to CSS custom property on root */
function applyFontSize(size: number) {
  document.documentElement.style.setProperty("--ui-font-size", `${size}rem`);
}

/** Apply contentWidth to CSS custom property on root */
function applyContentWidth(width: number) {
  document.documentElement.style.setProperty("--content-max-width", `${width}%`);
}

/** Apply user-controlled opacity to the DOM.
 *
 * Writes the raw 0..1 value to `--app-opacity` on `:root` for any
 * component that wants to bind to it.  The actual visual tint is
 * applied by AppLayout's `glassBg` (rgba with this alpha), not by
 * this variable — the html/body layers are intentionally transparent
 * so the OS-native vibrancy shows through. */
function applyOpacity(opacity: number) {
  document.documentElement.style.setProperty("--app-opacity", String(opacity));
}

/** Apply accent preset to the DOM.
 *
 * Two layers of effect:
 *   1. `--color-accent` CSS variable — used by buttons, links, sliders, etc.
 *   2. `accent-{id}` class on <html>  — selects the matching per-accent
 *      block in globals.css that overrides `--glass-tint-light/dark`
 *      with the preset's hue-shifted near-neutral.
 *
 * The macOS native vibrancy tint (set via the `set_window_effect`
 * Tauri command) is owned by AppLayout — its useEffect on
 * `[isDark, accentColor]` re-invokes the command whenever either
 * changes, passing the matching RGBA tuples from `accentPresets.ts`.
 * That keeps the Rust side stateless w.r.t. accent selection.
 */
function applyAccentPreset(preset: AccentPreset) {
  document.documentElement.style.setProperty("--color-accent", preset.hex);
  const classList = document.documentElement.classList;
  // Remove any existing `accent-*` class, then add the current one.
  // We iterate instead of using a regex because classList doesn't
  // support wildcard removal in older browsers (and Safari < 17
  // doesn't support `className.replace` with regex reliably either).
  for (const cls of Array.from(classList)) {
    if (cls.startsWith("accent-")) classList.remove(cls);
  }
  classList.add(`accent-${preset.id}`);
}

/** Read persisted theme from localStorage, fallback to "system" */
function getPersistedTheme(): Theme {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_THEME);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch {
    // localStorage unavailable (SSR / privacy mode)
  }
  return DEFAULT_THEME;
}

/** Read persisted font size from localStorage, fallback to 0.875 (M) */
function getPersistedFontSize(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FONT_SIZE);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val > 0) return val;
    }
  } catch { }
  return DEFAULT_FONT_SIZE;
}

/** Read persisted log level from localStorage, fallback to "info" */
function getPersistedLogLevel(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_LEVEL);
    if (stored) return stored;
  } catch { }
  return DEFAULT_LOG_LEVEL;
}

/** Read persisted content width from localStorage, fallback to 90 */
function getPersistedContentWidth(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_CONTENT_WIDTH);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 40 && val <= 100) return val;
    }
  } catch { }
  return DEFAULT_CONTENT_WIDTH;
}

/** Read persisted accent color from localStorage, fallback to default blue.
 *
 * Always returns a valid `#rrggbb` hex string.  Unknown / malformed values
 * are coerced to `DEFAULT_ACCENT_COLOR` so downstream code can rely on the
 * format.  The matching `AccentPreset` is resolved lazily via
 * `getAccentPresetByHex()` in `setAccentColor` — see `applyAccentPreset`.
 */
function getPersistedAccentColor(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_ACCENT_COLOR);
    if (stored && /^#[0-9a-fA-F]{6}$/.test(stored)) return stored;
  } catch { }
  return DEFAULT_ACCENT_COLOR;
}

/** Read persisted gateway URL from localStorage, fallback to DEFAULT_GATEWAY_URL */
function getPersistedGatewayUrl(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_GATEWAY_URL);
    if (stored) return stored;
  } catch { }
  return DEFAULT_GATEWAY_URL;
}

/** Read persisted gateway URL history (most-recent first). Deduplicates & caps to GATEWAY_URL_HISTORY_MAX. */
function getPersistedGatewayUrlHistory(): string[] {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_GATEWAY_URL_HISTORY);
    if (!stored) return [];
    const arr = JSON.parse(stored);
    if (!Array.isArray(arr)) return [];
    const seen = new Set<string>();
    const out: string[] = [];
    for (const v of arr) {
      if (typeof v !== "string") continue;
      const t = v.trim();
      if (!t || seen.has(t)) continue;
      seen.add(t);
      out.push(t);
      if (out.length >= GATEWAY_URL_HISTORY_MAX) break;
    }
    return out;
  } catch {
    return [];
  }
}

/** Read persisted gateway mode from localStorage, fallback to "local" */
function getPersistedGatewayMode(): GatewayMode {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_GATEWAY_MODE);
    if (stored === "local" || stored === "remote" || stored === "relay") return stored;
  } catch { }
  return DEFAULT_GATEWAY_MODE;
}

/** Read persisted auto-start-local-node flag; default ON (only missing
 *  key or an explicit "false" turns it off). */
function getPersistedAutoStartLocalNode(): boolean {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_AUTO_START_LOCAL_NODE);
    if (stored !== null) return stored !== "false";
  } catch { }
  return true;
}

/** Read persisted log file size from localStorage, fallback to 10 (MB) */
function getPersistedLogFileSizeMb(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_FILE_SIZE);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 0) return val;
    }
  } catch { }
  return DEFAULT_LOG_FILE_SIZE_MB;
}

/** Read persisted log file count from localStorage, fallback to 20 */
function getPersistedLogFileCount(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_FILE_COUNT);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 0) return val;
    }
  } catch { }
  return DEFAULT_LOG_FILE_COUNT;
}

/** Read persisted frontend log level from localStorage, fallback to "warn".
 * Valid values: trace, debug, info, warn, error, off */
function getPersistedFrontendLogLevel(): LogLevel {
  const valid: LogLevel[] = ["trace", "debug", "info", "warn", "error", "off"];
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FRONTEND_LOG_LEVEL);
    if (stored && valid.includes(stored as LogLevel)) return stored as LogLevel;
  } catch { }
  return DEFAULT_FRONTEND_LOG_LEVEL;
}

/** Read persisted opacity from localStorage, fallback to 1.0 (opaque) */
function getPersistedOpacity(): number {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_OPACITY);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val >= 0.0 && val <= 1.0) return val;
    }
  } catch { }
  return DEFAULT_OPACITY;
}

interface SettingsStore {
  theme: Theme;
  osTheme: "light" | "dark";
  fontSize: number;
  contentWidth: number;
  opacity: number;
  accentColor: string;
  gatewayUrl: string;
  /**
   * Recently-used gateway URLs (most-recent first). Lets the SettingsPage
   * render a combo dropdown and lets SplashScreen probe candidates when
   * the persisted URL is unreachable (e.g. laptop moved to a new LAN).
   */
  gatewayUrlHistory: string[];
  gatewayMode: GatewayMode;
  /**
   * Remote mode only: auto-start this machine's local Node on Desktop
   * launch when the machine is already enrolled (ADR-087 follow-up).
   * First-time enrollment is never silent — the AgentList banner asks.
   */
  autoStartLocalNode: boolean;
  logLevel: string;
  logFileSizeMb: number;
  logFileCount: number;
  frontendLogLevel: LogLevel;
  setTheme: (theme: Theme) => void;
  setFontSize: (size: number) => void;
  setContentWidth: (width: number) => void;
  setOpacity: (opacity: number) => void;
  setAccentColor: (color: string) => void;
  setGatewayUrl: (url: string) => void;
  /**
   * Persist a Gateway URL and keep the mode consistent with it in one
   * step. `relay` mode can only reach its Gateway through `https://`
   * relay-device domains (Rust `relay_mqtt_wss_url` rejects every other
   * scheme), so an `http://` URL picked while mode is `relay` (candidate
   * chooser, timeout-view edit) would otherwise create the exact combo
   * that leaves MQTT permanently rejected. Flips the mode to `remote` in
   * that case. No-ops when the URL is unchanged and no fix is needed.
   */
  applyGatewayUrl: (url: string) => void;
  /** Push a URL to the front of the history (LRU). No-ops for falsy / duplicate-of-front. */
  recordGatewayUrl: (url: string) => void;
  setGatewayMode: (mode: GatewayMode) => void;
  setAutoStartLocalNode: (on: boolean) => void;
  setLogLevel: (level: string) => void;
  setLogFileSizeMb: (size: number) => void;
  setLogFileCount: (count: number) => void;
  setFrontendLogLevel: (level: LogLevel) => void;
}

export const useSettingsStore = create<SettingsStore>((set, get) => {
  // Initialize from persisted values and apply theme to DOM immediately
  const initialTheme = getPersistedTheme();
  const initialOsTheme = readOsTheme();
  const initialFontSize = getPersistedFontSize();
  const initialLogLevel = getPersistedLogLevel();
  const initialFrontendLogLevel = getPersistedFrontendLogLevel();
  const initialOpacity = getPersistedOpacity();
  const initialContentWidth = getPersistedContentWidth();
  const initialAccentColor = getPersistedAccentColor();
  applyTheme(initialTheme, initialOsTheme);
  applyFontSize(initialFontSize);
  applyOpacity(initialOpacity);
  applyContentWidth(initialContentWidth);
  applyAccentPreset(getAccentPresetByHex(initialAccentColor) ?? DEFAULT_ACCENT_PRESET);
  // Sync logger module level with persisted setting on startup
  setLoggerLevel(initialFrontendLogLevel);

  // Subscribe to OS theme changes so theme="system" stays in sync.
  // Without this listener the .dark class on <html> is frozen at app
  // start, so Tailwind dark variants and any component reading the
  // resolved theme (AppLayout glass, NavBar, SetiIcon) stay stale when
  // the user switches macOS appearance while the app is running.
  if (typeof window !== "undefined" && window.matchMedia) {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handleChange = (e: MediaQueryListEvent) => {
      const nextOsTheme: "light" | "dark" = e.matches ? "dark" : "light";
      const current = get();
      if (current.osTheme === nextOsTheme) return;
      // Always update osTheme so any component subscribed via the store
      // re-renders with the new effective theme.
      // If current.theme === "system", also re-apply to DOM so the
      // .dark class on <html> flips and Tailwind dark variants kick in.
      const applyNeeded = current.theme === "system";
      set({ osTheme: nextOsTheme });
      if (applyNeeded) applyTheme(current.theme, nextOsTheme);
    };
    if (mq.addEventListener) {
      mq.addEventListener("change", handleChange);
    } else if ((mq as any).addListener) {
      // Safari < 14 fallback (not strictly needed since Tauri requires
      // macOS 11+/Safari 14+, but kept for safety)
      (mq as any).addListener(handleChange);
    }
  }

  return {
    theme: initialTheme,
    osTheme: initialOsTheme,
    fontSize: initialFontSize,
    contentWidth: initialContentWidth,
    opacity: initialOpacity,
    accentColor: initialAccentColor,
    gatewayUrl: getPersistedGatewayUrl(),
    gatewayUrlHistory: getPersistedGatewayUrlHistory(),
    gatewayMode: getPersistedGatewayMode(),
    autoStartLocalNode: getPersistedAutoStartLocalNode(),
    logLevel: initialLogLevel,
    logFileSizeMb: getPersistedLogFileSizeMb(),
    logFileCount: getPersistedLogFileCount(),
    frontendLogLevel: initialFrontendLogLevel,

    setTheme: (theme) => {
      const osTheme = get().osTheme;
      applyTheme(theme, osTheme);
      try { localStorage.setItem(STORAGE_KEY_THEME, theme); } catch { }
      set({ theme });
    },

    setFontSize: (fontSize) => {
      applyFontSize(fontSize);
      try { localStorage.setItem(STORAGE_KEY_FONT_SIZE, String(fontSize)); } catch { }
      set({ fontSize });
    },

    setContentWidth: (contentWidth) => {
      applyContentWidth(contentWidth);
      try { localStorage.setItem(STORAGE_KEY_CONTENT_WIDTH, String(contentWidth)); } catch { }
      set({ contentWidth });
    },

    setOpacity: (opacity) => {
      applyOpacity(opacity);
      try { localStorage.setItem(STORAGE_KEY_OPACITY, String(opacity)); } catch { }
      set({ opacity });
    },

    setAccentColor: (accentColor) => {
      // Resolve the preset by hex; fall back to the default if the
      // stored value is no longer a known preset (e.g. a removed accent).
      const preset = getAccentPresetByHex(accentColor) ?? DEFAULT_ACCENT_PRESET;
      applyAccentPreset(preset);
      try { localStorage.setItem(STORAGE_KEY_ACCENT_COLOR, accentColor); } catch { }
      set({ accentColor });
    },

    setGatewayUrl: (gatewayUrl) => {
      const oldUrl = get().gatewayUrl;
      try { localStorage.setItem(STORAGE_KEY_GATEWAY_URL, gatewayUrl); } catch { }
      set({ gatewayUrl });
      // Sync to Rust so subsequent Tauri commands use the new URL
      pushGatewayConfigToRust(get().gatewayMode, gatewayUrl);
      // NOTE: history is NOT updated here. The address hasn't necessarily
      // connected yet — a user typo or an unreachable host would otherwise
      // pollute the candidate list. History is updated by the connection
      // lifecycle subscriber (see SplashScreen / AppLayout), so only URLs
      // that ACTUALLY connected (or were just disconnected from) end up
      // in the LRU.

      // NEW: notify authStore when the URL actually changed. Skipped on
      // no-op writes (SettingsPage re-saves the same value, devtools
      // tweak) so we don't burn a probe round-trip for nothing. The
      // probe validates the existing access token against the new
      // Gateway: same Gateway behind an alias → keep session; different
      // Gateway (new signing key) → drop session and re-resolve the
      // auth mode. See `authStore.onGatewayUrlChanged` for the full
      // decision tree. Fixes the WiFi-hop / LAN-move "Node 不在网关里"
      // failure where stale tokens held by the old Gateway's HMAC key
      // turn every /api/* into 401.
      if (oldUrl !== gatewayUrl) {
        void useAuthStore.getState().onGatewayUrlChanged(gatewayUrl, oldUrl);
      }
    },
    /**
     * URL + mode atomic consistency (see interface docs). Composed of the
     * two existing actions so every side effect stays in one place
     * (localStorage write, Rust push, `onGatewayUrlChanged` probe): URL
     * first — its push already carries the new URL — then the mode fix,
     * which re-pushes the config with the URL updated. The LAST
     * `set_gateway_config` Rust sees is therefore (remote, url); the
     * intermediate (relay, url) push's follow-up `connect_mqtt` rejection
     * is swallowed by `pushGatewayConfigToRust` by design.
     */
    applyGatewayUrl: (gatewayUrl) => {
      const needsModeFix =
        get().gatewayMode === "relay" &&
        gatewayUrl.trim().toLowerCase().startsWith("http://");
      if (gatewayUrl === get().gatewayUrl && !needsModeFix) return;
      if (gatewayUrl !== get().gatewayUrl) {
        get().setGatewayUrl(gatewayUrl);
      }
      if (needsModeFix) {
        get().setGatewayMode("remote");
      }
    },
    /**
     * LRU-push: dedupe, cap, prepend. Called by setGatewayUrl so every
     * saved URL (combo box selection, SplashScreen timeout view edit,
     * candidate pick) is remembered without callers needing to opt in.
     */
    recordGatewayUrl: (url) => {
      const t = (url ?? "").trim();
      if (!t) return;
      const cur = get().gatewayUrlHistory;
      // No-op when it would just shuffle the front entry to itself — saves
      // a localStorage write on every SettingsPage keystroke that didn't
      // change the value.
      if (cur[0] === t) return;
      const next = [t, ...cur.filter((u) => u !== t)].slice(0, GATEWAY_URL_HISTORY_MAX);
      try { localStorage.setItem(STORAGE_KEY_GATEWAY_URL_HISTORY, JSON.stringify(next)); } catch { }
      set({ gatewayUrlHistory: next });
    },
    setGatewayMode: (gatewayMode) => {
      try { localStorage.setItem(STORAGE_KEY_GATEWAY_MODE, gatewayMode); } catch { }
      set({ gatewayMode });
      // Sync to Rust. For local→remote this also stops any locally-
      // spawned Gateway. For remote→local the user must press Start
      // (or reload) — we don't auto-spawn here.
      pushGatewayConfigToRust(gatewayMode, get().gatewayUrl);
    },
    setAutoStartLocalNode: (autoStartLocalNode) => {
      try { localStorage.setItem(STORAGE_KEY_AUTO_START_LOCAL_NODE, autoStartLocalNode ? "true" : "false"); } catch { }
      set({ autoStartLocalNode });
    },
    setLogLevel: (logLevel) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_LEVEL, logLevel); } catch { }
      set({ logLevel });
    },
    setLogFileSizeMb: (logFileSizeMb) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_FILE_SIZE, String(logFileSizeMb)); } catch { }
      set({ logFileSizeMb });
    },
    setLogFileCount: (logFileCount) => {
      try { localStorage.setItem(STORAGE_KEY_LOG_FILE_COUNT, String(logFileCount)); } catch { }
      set({ logFileCount });
    },
    setFrontendLogLevel: (frontendLogLevel) => {
      try { localStorage.setItem(STORAGE_KEY_FRONTEND_LOG_LEVEL, frontendLogLevel); } catch { }
      setLoggerLevel(frontendLogLevel);
      set({ frontendLogLevel });
    },
  };
});
