import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";

/**
 * Typed client for the native preview browser (`src-tauri/src/browser/`).
 *
 * Why this exists — the architectural fact:
 *
 *   An in-app `<iframe>` can NEVER display github.com, google.com, or any site
 *   that answers `X-Frame-Options: deny` / `Content-Security-Policy:
 *   frame-ancestors 'none'`. That restriction is enforced by the browser engine
 *   on the *embedded* document, so no frontend trick removes it (ADR-118).
 *
 *   The preview therefore runs as a **child native webview** created by
 *   `Webview::add_child` (`WebviewBuilder`) inside the main window. A child
 *   webview is a top-level browsing context, so framing headers do not apply —
 *   exactly what Cursor / VS Code "Simple Browser" does.
 *
 * Two consequences the UI must honour, both visible in the type signatures:
 *
 *   1. The native webview paints **above** all HTML. Nothing can be drawn over
 *      it, so every control (toolbar, spinner, error card) has to live outside
 *      its rectangle — see `WebPreviewHost`.
 *   2. Geometry is owned by the Rust side; React only reports `PreviewBounds`
 *      (logical px, relative to the window client area — the same space
 *      `getBoundingClientRect()` uses).
 */

/** Rectangle in logical pixels, relative to the window's client area. */
export interface PreviewBounds {
    x: number;
    y: number;
    width: number;
    height: number;
}

/** Load phase of a session; mirrors `Phase::as_str` on the Rust side. */
export type PreviewPhase = "loading" | "loaded";

/** One live preview webview. Mirrors `PreviewStateDto` (camelCase on the wire). */
export interface PreviewState {
    label: string;
    url: string;
    title: string | null;
    phase: PreviewPhase;
    visible: boolean;
}

/** `window.open` / `target="_blank"` inside a preview: the frontend opens a tab. */
export interface PreviewNewWindowRequest {
    url: string;
    /** Label of the session that requested it. */
    from: string;
}

export type PreviewHistoryDirection = "back" | "forward";

/** A preview page tried to download a file; the app refuses and offers the browser. */
export interface PreviewDownloadRequest {
    url: string;
    /** Label of the session that requested it. */
    from: string;
}

/** Emitted on every observable state change of any session. */
export const PREVIEW_STATE_EVENT = "preview://state";
/** Emitted when a page asks for a new window. */
export const PREVIEW_NEW_WINDOW_EVENT = "preview://new-window";
/** Emitted when a page asks to download a file (refused; see `host.rs`). */
export const PREVIEW_DOWNLOAD_EVENT = "preview://download";

/**
 * Error codes the Rust commands answer with, typed so the UI can branch on the
 * cause instead of pattern-matching free-form text.
 */
export type PreviewErrorCode =
    | "unknown_preview"
    | "blocked_scheme"
    | "invalid_url"
    | "create_failed"
    | "main_window_missing"
    | "other";

export interface PreviewError {
    code: PreviewErrorCode;
    /** Everything after the code, verbatim — useful in logs, not for the user. */
    detail: string;
}

const KNOWN_CODES: readonly PreviewErrorCode[] = [
    "unknown_preview",
    "blocked_scheme",
    "invalid_url",
    "create_failed",
    "main_window_missing",
];

/**
 * Normalise a rejected command into `{ code, detail }`.
 *
 * Tauri rejects with the `String` the command returned, so the code is the
 * prefix before the first `": "`. Anything unrecognised becomes `other` with
 * the raw text preserved — never swallowed.
 */
export function parsePreviewError(error: unknown): PreviewError {
    const raw =
        typeof error === "string"
            ? error
            : error instanceof Error
              ? error.message
              : String(error ?? "");
    const separator = raw.indexOf(": ");
    const head = (separator === -1 ? raw : raw.slice(0, separator)).trim() as PreviewErrorCode;
    if (KNOWN_CODES.includes(head)) {
        return { code: head, detail: separator === -1 ? "" : raw.slice(separator + 2) };
    }
    return { code: "other", detail: raw };
}

/**
 * `invoke` throws *synchronously* when the Tauri bridge is absent (browser dev,
 * jsdom), which would escape a `void …catch()` cleanup. Funnelling every call
 * through here keeps one failure shape: a rejected promise.
 */
function call<T>(command: string, args?: Record<string, unknown>): Promise<T> {
    try {
        return invoke<T>(command, args);
    } catch (error) {
        return Promise.reject(error);
    }
}

/** Create a session and its native webview. Async: `add_child` must not run on the UI thread. */
export function openPreview(url: string, bounds: PreviewBounds): Promise<PreviewState> {
    return call<PreviewState>("preview_open", { url, bounds });
}

/** Move/resize. A zero-sized rect parks (hides) the webview. */
export function setPreviewBounds(label: string, bounds: PreviewBounds): Promise<void> {
    return call<void>("preview_set_bounds", { label, bounds });
}

/** Show/hide without touching geometry (inactive tab, native overlay open). */
export function setPreviewVisible(label: string, visible: boolean): Promise<void> {
    return call<void>("preview_set_visible", { label, visible });
}

/** Point an existing session at a new URL (same policy as creation). */
export function navigatePreview(label: string, url: string): Promise<void> {
    return call<void>("preview_navigate", { label, url });
}

export function reloadPreview(label: string): Promise<void> {
    return call<void>("preview_reload", { label });
}

/** Drive the page's own History API (Tauri exposes no go_back/go_forward). */
export function previewHistory(label: string, direction: PreviewHistoryDirection): Promise<void> {
    return call<void>("preview_history", { label, direction });
}

/** Destroy a session and its webview. Idempotent. */
export function closePreview(label: string): Promise<void> {
    return call<void>("preview_close", { label });
}

/** Destroy every session — used when the editor panel unmounts. */
export function closeAllPreviews(): Promise<void> {
    return call<void>("preview_close_all");
}

/** Every live session, e.g. to rebuild tab state after a frontend reload. */
export function listPreviews(): Promise<PreviewState[]> {
    return call<PreviewState[]>("preview_list");
}

export function subscribePreviewState(
    handler: (state: PreviewState) => void,
): Promise<UnlistenFn> {
    return listen<PreviewState>(PREVIEW_STATE_EVENT, (event) => handler(event.payload));
}

export function subscribePreviewNewWindow(
    handler: (request: PreviewNewWindowRequest) => void,
): Promise<UnlistenFn> {
    return listen<PreviewNewWindowRequest>(PREVIEW_NEW_WINDOW_EVENT, (event) =>
        handler(event.payload),
    );
}

export function subscribePreviewDownload(
    handler: (request: PreviewDownloadRequest) => void,
): Promise<UnlistenFn> {
    return listen<PreviewDownloadRequest>(PREVIEW_DOWNLOAD_EVENT, (event) =>
        handler(event.payload),
    );
}

/**
 * Normalise user input from the address bar: bare hosts get `https://`.
 * Kept next to the rest of the protocol so both the toolbar and any future
 * caller share one rule.
 */
export function normalizePreviewUrl(input: string): string {
    const trimmed = input.trim();
    if (!trimmed) return "";
    if (/^[a-z][a-z0-9+.-]*:/i.test(trimmed)) return trimmed;
    return `https://${trimmed}`;
}