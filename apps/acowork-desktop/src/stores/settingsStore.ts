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
  DEFAULT_LOG_FILE_SIZE_MB,
  DEFAULT_LOG_FILE_COUNT,
  DEFAULT_FRONTEND_LOG_LEVEL,
} from "../lib/defaults";
import {
  setLevel as setLoggerLevel,
  type LogLevel,
} from "../lib/logger";
import { log } from "../lib/logger";
import {
  deriveGatewayMode,
  hasHttpScheme,
  isHttpsUrl,
  normalizeGatewayUrl,
} from "../lib/gatewayUrl";
import { useAuthStore } from "./authStore";
import { useConnectionStore } from "./connectionStore";
import { readPref, writePref, onScopeChange } from "../lib/prefsScope";
import {
  CUSTOM_ACCENT_ID,
  isCustomAccent,
  resolveAccentPreset,
  type AccentPreset,
} from "../lib/accentPresets";

/**
 * Push the connection config to the Rust backend so that:
 *   - All Tauri HTTP commands use the correct base URL
 *   - Rust knows whether spawning a local Gateway is allowed (derived mode)
 *   - An address change rebuilds the MQTT connection
 *
 * ADR-115: `mode` is derived from the URL by the caller, never persisted or
 * chosen by the user. Best-effort: errors are logged, never thrown — settings
 * persistence must not be blocked by a transient Tauri command failure.
 */
async function pushGatewayConfigToRust(mode: GatewayMode, url: string): Promise<void> {
  try {
    await invoke("set_gateway_config", {
      config: { mode, url },
    });
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
/** Last user-picked custom accent, kept separately from `acowork-accent-color`
 *  so selecting a shipped preset does not destroy it. */
const STORAGE_KEY_ACCENT_CUSTOM = "acowork-accent-custom";
/** The server address to connect to on the next launch. */
const STORAGE_KEY_GATEWAY_URL = "acowork-gateway-url";
const STORAGE_KEY_GATEWAY_URL_HISTORY = "acowork-gateway-url-history";
/** Last address that actually reached a connected Gateway. Shown first in
 *  the picker as the safe way back after a wrong address / wrong password. */
const STORAGE_KEY_LAST_GOOD_URL = "acowork-gateway-last-good-url";
/** The only explicit topology switch: the dedicated relay entry point. */
const STORAGE_KEY_RELAY_ACTIVE = "acowork-gateway-relay-active";
/** Remote-mode auto-start of the local Node on Desktop launch (ADR-087 follow-up). */
const STORAGE_KEY_AUTO_START_LOCAL_NODE = "acowork-auto-start-local-node";

/** Max retained gateway URLs in localStorage. */
const GATEWAY_URL_HISTORY_MAX = 8;
const STORAGE_KEY_LOG_FILE_SIZE = "acowork-log-file-size";
const STORAGE_KEY_LOG_FILE_COUNT = "acowork-log-file-count";
const STORAGE_KEY_FRONTEND_LOG_LEVEL = "acowork-frontend-log-level";

/** Read current OS theme preference from matchMedia */
function readOsTheme(): "light" | "dark" {
  if (typeof window === "undefined" || !window.matchMedia) return "light";
  return window.matchMedia("(prefers-color-scheme: dark)").matches ? "dark" : "light";
}

/** Apply theme to DOM by toggling .dark class on <html>. */
function applyTheme(theme: Theme, osTheme: "light" | "dark") {
  const effective = theme === "system" ? osTheme : theme;
  document.documentElement.classList.toggle("dark", effective === "dark");
}

function applyFontSize(size: number) {
  document.documentElement.style.setProperty("--ui-font-size", `${size}rem`);
}

function applyContentWidth(width: number) {
  document.documentElement.style.setProperty("--content-max-width", `${width}%`);
}

/** Apply user-controlled opacity to the DOM. */
function applyOpacity(opacity: number) {
  document.documentElement.style.setProperty("--app-opacity", String(opacity));
}

/** Apply accent preset to the DOM. */
function applyAccentPreset(preset: AccentPreset) {
  document.documentElement.style.setProperty("--color-accent", preset.hex);
  const classList = document.documentElement.classList;
  for (const cls of Array.from(classList)) {
    if (cls.startsWith("accent-")) classList.remove(cls);
  }
  classList.add(`accent-${preset.id}`);

  const style = document.documentElement.style;
  if (preset.id === CUSTOM_ACCENT_ID) {
    style.setProperty("--glass-tint-light", preset.glassTintLight);
    style.setProperty("--glass-tint-dark", preset.glassTintDark);
  } else {
    style.removeProperty("--glass-tint-light");
    style.removeProperty("--glass-tint-dark");
  }
}

function getPersistedTheme(): Theme {
  try {
    const stored = readPref(STORAGE_KEY_THEME);
    if (stored === "light" || stored === "dark" || stored === "system") return stored;
  } catch { }
  return DEFAULT_THEME;
}

function getPersistedFontSize(): number {
  try {
    const stored = readPref(STORAGE_KEY_FONT_SIZE);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val > 0) return val;
    }
  } catch { }
  return DEFAULT_FONT_SIZE;
}

function getPersistedLogLevel(): string {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_LOG_LEVEL);
    if (stored) return stored;
  } catch { }
  return DEFAULT_LOG_LEVEL;
}

function getPersistedContentWidth(): number {
  try {
    const stored = readPref(STORAGE_KEY_CONTENT_WIDTH);
    if (stored) {
      const val = parseInt(stored, 10);
      if (!isNaN(val) && val >= 40 && val <= 100) return val;
    }
  } catch { }
  return DEFAULT_CONTENT_WIDTH;
}

function getPersistedOpacity(): number {
  try {
    const stored = readPref(STORAGE_KEY_OPACITY);
    if (stored) {
      const val = parseFloat(stored);
      if (!isNaN(val) && val >= 0.0 && val <= 1.0) return val;
    }
  } catch { }
  return DEFAULT_OPACITY;
}

/** A `#rrggbb` in the exact shape `--color-accent` requires. */
function isAccentHex(value: string): boolean {
  return /^#[0-9a-fA-F]{6}$/.test(value);
}

function getPersistedAccentColor(): string {
  try {
    const stored = readPref(STORAGE_KEY_ACCENT_COLOR);
    if (stored && isAccentHex(stored)) return stored;
  } catch { }
  return DEFAULT_ACCENT_COLOR;
}

function getPersistedAccentCustom(): string | null {
  try {
    const stored = readPref(STORAGE_KEY_ACCENT_CUSTOM);
    if (!stored || !isAccentHex(stored)) return null;
    return isCustomAccent(stored) ? stored : null;
  } catch { }
  return null;
}

/** The address boot will connect to. Malformed / empty falls back to localhost. */
function getPersistedGatewayUrl(): string {
  try {
    const stored = normalizeGatewayUrl(localStorage.getItem(STORAGE_KEY_GATEWAY_URL) ?? "");
    if (stored) return stored;
  } catch { }
  return DEFAULT_GATEWAY_URL;
}

/** Read persisted gateway URL history (most-recent first). Deduplicates & caps. */
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
      const t = normalizeGatewayUrl(v);
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

function getPersistedLastGoodUrl(): string | null {
  try {
    const stored = normalizeGatewayUrl(localStorage.getItem(STORAGE_KEY_LAST_GOOD_URL) ?? "");
    return stored || null;
  } catch {
    return null;
  }
}

function getPersistedRelayActive(): boolean {
  try {
    return localStorage.getItem(STORAGE_KEY_RELAY_ACTIVE) === "true";
  } catch {
    return false;
  }
}

/** Read persisted auto-start-local-node flag; default ON. */
function getPersistedAutoStartLocalNode(): boolean {
  try {
    const stored = localStorage.getItem(STORAGE_KEY_AUTO_START_LOCAL_NODE);
    if (stored !== null) return stored !== "false";
  } catch { }
  return true;
}

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

function getPersistedFrontendLogLevel(): LogLevel {
  const valid: LogLevel[] = ["trace", "debug", "info", "warn", "error", "off"];
  try {
    const stored = localStorage.getItem(STORAGE_KEY_FRONTEND_LOG_LEVEL);
    if (stored && valid.includes(stored as LogLevel)) return stored as LogLevel;
  } catch { }
  return DEFAULT_FRONTEND_LOG_LEVEL;
}

interface SettingsStore {
  theme: Theme;
  osTheme: "light" | "dark";
  fontSize: number;
  contentWidth: number;
  opacity: number;
  accentColor: string;
  customAccent: string | null;

  /** The one user-facing connection setting: the Gateway server address. */
  gatewayUrl: string;
  /** Addresses that actually connected, most-recent first. */
  gatewayUrlHistory: string[];
  /** Last address that reached `connected`. Never a typo or a dead host. */
  lastGoodUrl: string | null;
  /**
   * DERIVED, never written by a UI control. `local` ⇔ loopback URL,
   * `remote` ⇔ any other address, `relay` ⇔ the dedicated relay entry.
   * Kept in the store so the existing Rust/MQTT/ACL consumers have one
   * consistent verdict.
   */
  gatewayMode: GatewayMode;
  /** Explicit relay opt-in, set only by `setRelayGateway`. */
  relayActive: boolean;
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
  /** Low-level address write for runtime auto-heal / boot retry. Does NOT
   *  restart the boot pipeline or drop the session. */
  setGatewayUrl: (url: string) => void;
  /** User picked a server address (login / setup / splash / settings).
   *  Clears the session and re-runs the whole connection pipeline. */
  switchGateway: (url: string) => Promise<void>;
  /** Dedicated relay entry point. Relay URLs are https device domains. */
  setRelayGateway: (url: string) => Promise<void>;
  /** Adopt the base_url Rust resolved after spawning a local Gateway. */
  adoptBootUrl: (url: string) => void;
  /** LRU-push + remember as last good. Called on a CONNECTED transition. */
  recordGatewayUrl: (url: string) => void;
  /** Forget every connection setting and reboot against localhost. */
  resetConnectionSettings: () => Promise<void>;
  setAutoStartLocalNode: (on: boolean) => void;
  setLogLevel: (level: string) => void;
  setLogFileSizeMb: (size: number) => void;
  setLogFileCount: (count: number) => void;
  setFrontendLogLevel: (level: LogLevel) => void;
}

export const useSettingsStore = create<SettingsStore>((set, get) => {
  const initialTheme = getPersistedTheme();
  const initialOsTheme = readOsTheme();
  const initialFontSize = getPersistedFontSize();
  const initialLogLevel = getPersistedLogLevel();
  const initialFrontendLogLevel = getPersistedFrontendLogLevel();
  const initialOpacity = getPersistedOpacity();
  const initialContentWidth = getPersistedContentWidth();
  const initialAccentColor = getPersistedAccentColor();
  const initialCustomAccent = getPersistedAccentCustom();
  const initialGatewayUrl = getPersistedGatewayUrl();
  // Relay is TLS-only (see `commitGatewayUrl`). Sanitize at init as well, so a
  // stale `acowork-gateway-relay-active=true` left behind next to a plain
  // http address can never resurrect the "relay + http://" pair on boot.
  const initialRelayActive = getPersistedRelayActive() && isHttpsUrl(initialGatewayUrl);

  applyTheme(initialTheme, initialOsTheme);
  applyFontSize(initialFontSize);
  applyOpacity(initialOpacity);
  applyContentWidth(initialContentWidth);
  applyAccentPreset(resolveAccentPreset(initialAccentColor));
  setLoggerLevel(initialFrontendLogLevel);

  if (typeof window !== "undefined" && window.matchMedia) {
    const mq = window.matchMedia("(prefers-color-scheme: dark)");
    const handleChange = (e: MediaQueryListEvent) => {
      const nextOsTheme: "light" | "dark" = e.matches ? "dark" : "light";
      const current = get();
      if (current.osTheme === nextOsTheme) return;
      const applyNeeded = current.theme === "system";
      set({ osTheme: nextOsTheme });
      if (applyNeeded) applyTheme(current.theme, nextOsTheme);
    };
    if (mq.addEventListener) {
      mq.addEventListener("change", handleChange);
    } else if ((mq as any).addListener) {
      (mq as any).addListener(handleChange);
    }
  }

  /**
   * The single choke point for an address change: normalize, derive the
   * mode, persist, push to Rust, and (optionally) re-validate the session.
   * `notifyAuth` is false for user-driven switches because those restart
   * the boot pipeline, which re-runs `authStore.init()` anyway.
   *
   * Returns `true` when the address was committed. Callers that own a form
   * (`GatewayAddressPicker`, the relay entry) use the `false` answer to keep
   * a bad value visible instead of pretending it was saved.
   */
  const commitGatewayUrl = (
    raw: string,
    relayActive: boolean,
    notifyAuth: boolean,
  ): boolean => {
    const url = normalizeGatewayUrl(raw) || DEFAULT_GATEWAY_URL;
    // An address with no http(s) scheme is not a Gateway URL — it is a host
    // that `fetch`/`base_url` can never resolve. Persisting one would make
    // the NEXT launch start by trying an address that cannot work, so the
    // store (the final line of defence) refuses it outright rather than
    // letting each caller remember to validate.
    if (!hasHttpScheme(url)) {
      log.warn("[settings] refusing a scheme-less Gateway address:", raw);
      return false;
    }
    const oldUrl = get().gatewayUrl;
    // Invariant: relay only exists over TLS device domains. Any write of a
    // plain http(s-less) address silently drops it, so the store can never
    // hold the "relay + http://" pair Rust rejects forever (MQTT would be
    // dead while /health still answers — the classic false "connected").
    const relay = relayActive && isHttpsUrl(url);
    const mode = deriveGatewayMode(url, relay);
    try { localStorage.setItem(STORAGE_KEY_GATEWAY_URL, url); } catch { }
    try {
      localStorage.setItem(STORAGE_KEY_RELAY_ACTIVE, relay ? "true" : "false");
    } catch { }
    set({ gatewayUrl: url, gatewayMode: mode, relayActive: relay });
    void pushGatewayConfigToRust(mode, url);
    if (notifyAuth && oldUrl !== url) {
      void useAuthStore.getState().onGatewayUrlChanged(url, oldUrl);
    }
    return true;
  };

  /** Clearing the session is mandatory: tokens are signed by one Gateway. */
  const dropSessionAndReboot = (): void => {
    useAuthStore.getState().clearLocalSession();
    useConnectionStore.getState().restart();
  };

  return {
    theme: initialTheme,
    osTheme: initialOsTheme,
    fontSize: initialFontSize,
    contentWidth: initialContentWidth,
    opacity: initialOpacity,
    accentColor: initialAccentColor,
    customAccent: initialCustomAccent,
    gatewayUrl: initialGatewayUrl,
    gatewayUrlHistory: getPersistedGatewayUrlHistory(),
    lastGoodUrl: getPersistedLastGoodUrl(),
    gatewayMode: deriveGatewayMode(initialGatewayUrl, initialRelayActive),
    relayActive: initialRelayActive,
    autoStartLocalNode: getPersistedAutoStartLocalNode(),
    logLevel: initialLogLevel,
    logFileSizeMb: getPersistedLogFileSizeMb(),
    logFileCount: getPersistedLogFileCount(),
    frontendLogLevel: initialFrontendLogLevel,

    setTheme: (theme) => {
      applyTheme(theme, get().osTheme);
      writePref(STORAGE_KEY_THEME, theme);
      set({ theme });
    },

    setFontSize: (fontSize) => {
      applyFontSize(fontSize);
      writePref(STORAGE_KEY_FONT_SIZE, String(fontSize));
      set({ fontSize });
    },

    setContentWidth: (contentWidth) => {
      applyContentWidth(contentWidth);
      writePref(STORAGE_KEY_CONTENT_WIDTH, String(contentWidth));
      set({ contentWidth });
    },

    setOpacity: (opacity) => {
      applyOpacity(opacity);
      writePref(STORAGE_KEY_OPACITY, String(opacity));
      set({ opacity });
    },

    setAccentColor: (accentColor) => {
      const value = isAccentHex(accentColor) ? accentColor : DEFAULT_ACCENT_COLOR;
      applyAccentPreset(resolveAccentPreset(value));
      writePref(STORAGE_KEY_ACCENT_COLOR, value);
      const custom = isCustomAccent(value) ? value : get().customAccent;
      if (isCustomAccent(value)) writePref(STORAGE_KEY_ACCENT_CUSTOM, value);
      set({ accentColor: value, customAccent: custom });
    },

    setGatewayUrl: (url) => commitGatewayUrl(url, get().relayActive, true),

    switchGateway: async (url) => {
      // Empty input is a user mistake, not a request to go back to the
      // default: `commitGatewayUrl` falls back to `DEFAULT_GATEWAY_URL` so the
      // store is never address-less, and swallowing an empty switch here is
      // what keeps that fallback out of user-driven paths.
      if (!normalizeGatewayUrl(url)) return;
      // Picking a plain address always leaves relay. Keeping relayActive
      // would recreate the old "relay + http://" combo that Rust rejects.
      // A refused address must not reboot: that would drop the session and
      // re-run the boot pipeline against the address we just rejected.
      if (!commitGatewayUrl(url, false, false)) return;
      dropSessionAndReboot();
    },

    setRelayGateway: async (url) => {
      // Refuse rather than silently downgrade: `commitGatewayUrl` drops relay
      // for a non-https address, so accepting one here would look like "relay
      // applied" while actually connecting as a plain remote Gateway. The
      // settings UI already blocks the submit for this case.
      if (!isHttpsUrl(url)) return;
      if (!commitGatewayUrl(url, true, false)) return;
      dropSessionAndReboot();
    },

    adoptBootUrl: (url) => {
      if (!normalizeGatewayUrl(url)) return;
      commitGatewayUrl(url, get().relayActive, false);
    },

    recordGatewayUrl: (url) => {
      const t = normalizeGatewayUrl(url);
      if (!t) return;
      const cur = get().gatewayUrlHistory;
      if (cur[0] !== t) {
        const next = [t, ...cur.filter((u) => u !== t)].slice(0, GATEWAY_URL_HISTORY_MAX);
        try {
          localStorage.setItem(STORAGE_KEY_GATEWAY_URL_HISTORY, JSON.stringify(next));
        } catch { }
        set({ gatewayUrlHistory: next });
      }
      try { localStorage.setItem(STORAGE_KEY_LAST_GOOD_URL, t); } catch { }
      set({ lastGoodUrl: t });
    },

    resetConnectionSettings: async () => {
      for (const key of [
        STORAGE_KEY_GATEWAY_URL,
        STORAGE_KEY_GATEWAY_URL_HISTORY,
        STORAGE_KEY_LAST_GOOD_URL,
        STORAGE_KEY_RELAY_ACTIVE,
      ]) {
        try { localStorage.removeItem(key); } catch { }
      }
      set({
        gatewayUrl: DEFAULT_GATEWAY_URL,
        gatewayUrlHistory: [],
        lastGoodUrl: null,
        relayActive: false,
        gatewayMode: deriveGatewayMode(DEFAULT_GATEWAY_URL, false),
      });
      await pushGatewayConfigToRust("local", DEFAULT_GATEWAY_URL);
      dropSessionAndReboot();
    },

    setAutoStartLocalNode: (autoStartLocalNode) => {
      try {
        localStorage.setItem(
          STORAGE_KEY_AUTO_START_LOCAL_NODE,
          autoStartLocalNode ? "true" : "false",
        );
      } catch { }
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

/**
 * Re-read the user-level preferences under the current account scope.
 * Machine-level connection settings (address / history / relay flag) are
 * deliberately left alone — the login round-trip itself depends on them.
 */
function rehydrateUserPrefs(): void {
  const theme = getPersistedTheme();
  const osTheme = readOsTheme();
  const fontSize = getPersistedFontSize();
  const contentWidth = getPersistedContentWidth();
  const opacity = getPersistedOpacity();
  const accentColor = getPersistedAccentColor();
  const customAccent = getPersistedAccentCustom();
  applyTheme(theme, osTheme);
  applyFontSize(fontSize);
  applyOpacity(opacity);
  applyContentWidth(contentWidth);
  applyAccentPreset(resolveAccentPreset(accentColor));
  useSettingsStore.setState({ theme, osTheme, fontSize, contentWidth, opacity, accentColor, customAccent });
}

onScopeChange(() => rehydrateUserPrefs());
