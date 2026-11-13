import type { Theme } from "./types";

/** The fixed loopback address of a Desktop-spawned Gateway. It is also the
 *  first entry in the server-address list — not a separate "local mode". */
export const DEFAULT_GATEWAY_URL = "http://127.0.0.1:19876";
export const DEFAULT_THEME: Theme = "system";
export const DEFAULT_FONT_SIZE = 0.875;
export const DEFAULT_LOG_LEVEL = "info";
export const DEFAULT_FRONTEND_LOG_LEVEL = "warn";
export const DEFAULT_CONTENT_WIDTH = 90;
export const DEFAULT_OPACITY = 0.8;
export const DEFAULT_ACCENT_COLOR = "#3b82f6";
export const DEFAULT_LOG_FILE_SIZE_MB = 10;
export const DEFAULT_LOG_FILE_COUNT = 20;
