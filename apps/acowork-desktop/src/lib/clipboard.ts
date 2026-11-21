// src/lib/clipboard.ts
//
// Unified clipboard helpers used by every context-menu "Copy" item and any
// other ad-hoc copy surface (CodeBlock, ErrorBox, AppLayout status, the image
// lightbox, etc.).
//
// Why a single helper instead of 5 inlined `navigator.clipboard.writeText`
// blocks?
//
//   1. This module owns the `navigator.clipboard` PATCH as well as the
//      helpers. WKWebView throws `NotAllowedError` when a write is not
//      triggered by a user gesture, and Monaco fires clipboard writes on every
//      keydown, which floods the console — so the patch swallows that error.
//      Swallowing is right for Monaco's noise and WRONG as a general answer: a
//      swallowed rejection resolves as a successful write, so any caller that
//      reports "copied" would lie. That is why `installClipboardPatches()`
//      keeps the unpatched functions and the helpers below call those,
//      falling back to `execCommand("copy")` on a real refusal.
//
//   2. Every context-menu "Copy" item has a recurring two-step pattern:
//        a. Snapshot the user's current text selection at the moment they
//           right-click. (Reading `window.getSelection()` at click time is
//           unreliable — `<button>` focus on mouseup clears the selection in
//           WebKit, so `selectedText` ends up empty by the time the button's
//           onClick fires. See the MessageBubble bug report.)
//        b. Fall back to a caller-supplied string when the selection is
//           empty, so the menu item works even without a prior text-drag.
//
//   This module owns that two-step pattern + the navigator.clipboard +
//   execCommand fallback. Callers stay focused on *what* to copy, not
//   *how*.
//
// Functions:
//   - `installClipboardPatches()`         — call ONCE from main.tsx, before
//                                           anything can copy (Monaco noise
//                                           suppression, originals preserved)
//   - `snapshotSelection()`               — read selection RIGHT NOW
//   - `copyText(text)`                    — best-effort copy one string
//   - `copySelectionOrFallback(fallback)` — snapshot selection, fall back
//                                           when empty, copy result
//   - `copyImageUrl(url)`                 — copy an image as PNG; `false` is a
//                                           real failure, never a swallowed one
//   - `wasCopiedRecently()`               — cheap heuristic for toast/UI
//                                           feedback (not used yet, kept
//                                           for future surfaces)

/**
 * The clipboard functions this module calls.
 *
 * Captured from the navigator as it was BEFORE our patch went in, so a helper
 * can see a rejected write instead of the patched silence.
 */
interface NativeClipboard {
  writeText?: (text: string) => Promise<void>;
  write?: (items: ClipboardItem[]) => Promise<void>;
  readText?: () => Promise<string>;
  read?: () => Promise<ClipboardItem[]>;
}

/** The pre-patch originals, once `installClipboardPatches()` has run. */
let patchedNatives: NativeClipboard | null = null;

/** Snapshot a clipboard object into our own record (bound functions, not the
 *  call-time lookup a patched object would hand back). */
function readClipboard(nav: Clipboard): NativeClipboard {
  // Each method is optional on purpose: a partial implementation (a test stub,
  // an older WebView that ships `writeText` only) must degrade to "this path
  // is unavailable" instead of throwing on a missing `.bind`.
  return {
    writeText: nav.writeText?.bind(nav),
    write: nav.write?.bind(nav),
    readText: nav.readText?.bind(nav),
    read: nav.read?.bind(nav),
  };
}

/**
 * The clipboard API the helpers below use: the pre-patch originals when the
 * patch is installed, otherwise whatever `navigator.clipboard` is right now.
 *
 * Resolved at CALL time (not module load) on purpose: a host that swaps the
 * clipboard implementation after this module was imported — a test stub, a
 * future platform shim — must still be honoured.
 */
function clipboardApi(): NativeClipboard | null {
  if (patchedNatives) return patchedNatives;
  if (typeof navigator === "undefined" || !navigator.clipboard) return null;
  return readClipboard(navigator.clipboard);
}

/** Suppress `NotAllowedError`, re-throw everything else. */
function suppressNotAllowed(promise: Promise<unknown>): Promise<unknown> {
  return promise.catch(function (err: unknown) {
    if (err instanceof DOMException && err.name === "NotAllowedError") {
      return undefined;
    }
    throw err;
  });
}

/**
 * Install the WKWebView clipboard patch. Call once from `main.tsx`, before
 * anything can copy.
 *
 * Why the patch exists: Monaco's `BrowserClipboardService` calls
 * `navigator.clipboard.write()` on every keydown/click (a WebKit workaround,
 * clipboardService.js:81-87) and `writeText`/`readText` during normal
 * operation. In WKWebView those throw `NotAllowedError` when not triggered by
 * a user gesture, which Monaco logs via `console.error`. Swallowing the error
 * is safe there: Monaco's `writeText` falls back to `execCommand("copy")` on
 * failure and its `readText` falls back to returning `""`.
 *
 * Why the patch does NOT wrap the helpers: see the header. `patchedNatives`
 * keeps the originals so `copyText` / `copyImageUrl` still observe a refusal.
 */
export function installClipboardPatches(): void {
  if (typeof navigator === "undefined" || !navigator.clipboard) return;
  const originals = readClipboard(navigator.clipboard);
  patchedNatives = originals;
  const native = navigator.clipboard;
  if (originals.writeText) {
    const writeText = originals.writeText;
    native.writeText = function (text: string) {
      return suppressNotAllowed(writeText(text)) as Promise<void>;
    };
  }
  if (originals.readText) {
    const readText = originals.readText;
    native.readText = function () {
      return suppressNotAllowed(readText()) as Promise<string>;
    };
  }
  if (originals.write) {
    const write = originals.write;
    native.write = function (items: ClipboardItem[]) {
      return suppressNotAllowed(write(items)) as Promise<void>;
    };
  }
  if (originals.read) {
    const read = originals.read;
    native.read = function () {
      return suppressNotAllowed(read()) as Promise<ClipboardItem[]>;
    };
  }
}

/**
 * Read the current document selection as plain text.
 *
 * IMPORTANT: only call this at the moment of the user gesture that needs it
 * (typically the right-click handler). Calling it later — e.g. inside a
 * button onClick — is unreliable in WKWebView because clicking a `<button>`
 * shifts focus to the button, which in turn clears the page's text
 * selection. The MessageBubble bug exists exactly because the original
 * implementation deferred this read to button click time.
 */
export function snapshotSelection(): string {
  if (typeof window === "undefined") return "";
  return window.getSelection()?.toString() ?? "";
}

/**
 * Copy `text` to the system clipboard.
 *
 * Tries the async Clipboard API first (through the pre-patch original, so a
 * real `NotAllowedError` is visible), then falls back to the classic
 * `document.execCommand("copy")` textarea trick — which works in WKWebView
 * when triggered from a user gesture even when the async clipboard API
 * refuses.
 *
 * Returns `true` when something was written, `false` when the text was
 * empty or both paths threw. Never throws — callers can fire-and-forget.
 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false;

  // Path 1: async Clipboard API. Going through the patched
  // `navigator.clipboard.writeText` instead would resolve as a success and
  // report a copy that never happened.
  try {
    const api = clipboardApi();
    if (!api?.writeText) throw new Error("clipboard unavailable");
    await api.writeText(text);
    return true;
  } catch {
    // fall through to execCommand
  }

  // Path 2: hidden-textarea + execCommand. Works in Tauri WebView as long
  // as we are still inside the original user-gesture event chain (we are,
  // because this function is invoked synchronously from button onClick).
  try {
    const ta = document.createElement("textarea");
    ta.value = text;
    ta.setAttribute("readonly", "");
    ta.style.position = "fixed";
    ta.style.left = "-9999px";
    ta.style.top = "0";
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    document.body.removeChild(ta);
    return ok;
  } catch {
    return false;
  }
}

/**
 * Copy the user's current text selection, or `fallback` when the selection
 * is empty/whitespace-only.
 *
 * This is the single helper every context-menu "Copy" item should call:
 *
 *   onClick: () => {
 *     void copySelectionOrFallback("the whole message body");
 *   }
 *
 * Callers SHOULD snapshot the selection at right-click time (so the menu
 * knows whether to enable the item) — `useContextMenu` exposes
 * `selectionAtOpen` for exactly this.
 */
export async function copySelectionOrFallback(fallback: string): Promise<boolean> {
  const selection = snapshotSelection();
  const text = selection.trim() || fallback;
  return copyText(text);
}

/**
 * Repaint any raster into PNG.
 *
 * `ClipboardItem` only guarantees `image/png` across platforms, so a JPEG or
 * WebP source has to go through a canvas first. Returns `null` when the
 * platform cannot repaint (no `createImageBitmap` / `OffscreenCanvas`), which
 * the caller reports as a failed copy rather than writing the wrong type.
 */
async function toPng(blob: Blob): Promise<Blob | null> {
  if (typeof createImageBitmap === "undefined" || typeof OffscreenCanvas === "undefined") {
    return null;
  }
  try {
    const bitmap = await createImageBitmap(blob);
    const canvas = new OffscreenCanvas(bitmap.width, bitmap.height);
    const ctx = canvas.getContext("2d");
    if (!ctx) return null;
    ctx.drawImage(bitmap, 0, 0);
    return await canvas.convertToBlob({ type: "image/png" });
  } catch {
    return null;
  }
}

/**
 * Copy the image behind `url` (`blob:` or `data:`) to the system clipboard as
 * PNG.
 *
 * Returns whether the bytes really reached the clipboard, and that answer is
 * the whole reason this lives here: `navigator.clipboard.write` is patched to
 * swallow `NotAllowedError`, so an inline `await navigator.clipboard.write(...)`
 * in a component resolves even when WKWebView refused the write — and the
 * surface would show "已复制" over an empty clipboard.
 *
 * The URL is the caller's: no auth or Gateway knowledge here, the surface has
 * already resolved the bytes.
 */
export async function copyImageUrl(url: string): Promise<boolean> {
  const api = clipboardApi();
  if (!api?.write || typeof ClipboardItem === "undefined") return false;
  try {
    const source = await (await fetch(url)).blob();
    const png = source.type === "image/png" ? source : await toPng(source);
    if (!png) return false;
    await api.write([new ClipboardItem({ "image/png": png })]);
    return true;
  } catch {
    return false;
  }
}