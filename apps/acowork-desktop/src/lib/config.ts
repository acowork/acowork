/**
 * Centralized Gateway configuration helpers.
 *
 * Default values are defined in `./defaults.ts` (a pure-constants module with
 * no runtime imports) and re-exported here for existing callers that import
 * `DEFAULT_*` from `./config`.
 *
 * ADR-115: the user-facing configuration is a single server address. The
 * deployment mode is derived from it (see `./gatewayUrl.ts`) and is never
 * persisted or written by a UI control.
 */

import { useSettingsStore } from "../stores/settingsStore";
import { DEFAULT_GATEWAY_URL } from "./defaults";
import { isLoopbackGatewayUrl } from "./gatewayUrl";
import type { GatewayMode } from "./types";

export {
  DEFAULT_GATEWAY_URL,
  DEFAULT_THEME,
  DEFAULT_FONT_SIZE,
  DEFAULT_LOG_LEVEL,
  DEFAULT_CONTENT_WIDTH,
  DEFAULT_OPACITY,
  DEFAULT_ACCENT_COLOR,
  DEFAULT_LOG_FILE_SIZE_MB,
  DEFAULT_LOG_FILE_COUNT,
} from "./defaults";

/**
 * Get the current Gateway URL.
 * Reads from settingsStore if available (user-configured), falls back to DEFAULT_GATEWAY_URL.
 */
export function getGatewayUrl(): string {
  try {
    const url = useSettingsStore.getState().gatewayUrl;
    if (url) return url;
  } catch {
    // settingsStore not yet available (e.g. SSR), fall through to default
  }
  return DEFAULT_GATEWAY_URL;
}

/**
 * Check if the current Gateway URL points to this machine.
 *
 * Used by local-only features (e.g. "Reveal in File Explorer" — opening
 * the OS file manager on a remote Gateway host has no value to the user)
 * and by the boot path to decide whether a local Gateway may be spawned.
 * Debug Protocol itself is NOT local-only — RPC rides Gateway HTTP and
 * events ride MQTT with the broker host derived from the Gateway URL.
 */
export function isGatewayLocal(): boolean {
  return isLoopbackGatewayUrl(getGatewayUrl());
}

/**
 * The derived internal topology mode. Read-only: there is no setter.
 * Relay keeps its own explicit entry point (`settingsStore.setRelayGateway`);
 * every other address maps to `local` (loopback) or `remote` (anything else).
 */
export function getGatewayMode(): GatewayMode {
  try {
    return useSettingsStore.getState().gatewayMode;
  } catch {
    return "local";
  }
}

/** True when the configured address is off this machine (LAN or relay). */
export function isGatewayModeRemote(): boolean {
  return getGatewayMode() !== "local";
}
