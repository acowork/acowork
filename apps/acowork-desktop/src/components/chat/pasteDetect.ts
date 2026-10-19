/**
 * Shared clipboard-paste detection for chat input boxes (execute session
 * ChatPanel and Chat-session AgentChatView).
 *
 * Detection is the single source of truth; the upload pipeline is the
 * consumer's business (ChatPanel uploads by filesystem path through the
 * Rust `upload_file` command; the Chat composer uploads `File` objects
 * through the Gateway's chat-attachment HTTP route).
 *
 * Why we always defer to the Rust clipboard reader instead of doing
 * frontend text-shape heuristics: complex agent messages (mermaid,
 * tables, code blocks) routinely contain lines like `/api/v1/users`,
 * `D:/data/file.txt`, or `file:///C:/...` inside markdown. Heuristics
 * that match on `s.startsWith("/")` or `^[A-Za-z]:[\\/]` mis-classify
 * those as file paths and trigger uploads against strings that are
 * not real files — producing `os error 53/161` floods and React
 * duplicate-key warnings when multiple false positives fire in the
 * same millisecond. Only the OS clipboard API can definitively say
 * "these are real files", so we treat it as the single source of
 * truth and skip the frontend heuristics entirely.
 */
import { invoke } from "@tauri-apps/api/core";
import { log } from "../../lib/logger";
import { extensionForMime } from "./clipboardImageExt";

/** Outcome of inspecting a paste event's clipboard contents. */
export type PasteOutcome =
  /** The OS clipboard holds real files (CF_HDROP / NSFilenames / uri-list). */
  | { kind: "files"; paths: string[] }
  /** Plain text — insert at the caret. */
  | { kind: "text"; text: string }
  /** A raster image (screenshot Ctrl+C). Blob + extension, no temp file. */
  | { kind: "image"; blob: Blob; extension: string }
  /** Last-resort: some WebViews populate `cd.files` with name-only Files
   *  (no OS path, unreadable) — surface the names as text. */
  | { kind: "fileNames"; names: string[] }
  /** Clipboard held nothing usable — silent no-op. */
  | { kind: "nothing" };

/**
 * Classify the paste event's clipboard contents. The CALLER is
 * responsible for `e.preventDefault()` (ChatPanel's double-paste rule:
 * every handled branch inserts content itself, so the browser default
 * must not run alongside it).
 */
export async function detectPaste(cd: DataTransfer): Promise<PasteOutcome> {
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
    log.debug("[pasteDetect] get_clipboard_file_paths failed", err);
  }
  paths = paths.filter((p): p is string => typeof p === "string" && p.length > 0);

  if (paths.length > 0) {
    // OS says these are real files — the consumer uploads them. We
    // deliberately do NOT also insert the `text/plain` here: when
    // Explorer copies N files, the text payload is exactly the N paths
    // joined by newlines, and inserting them would duplicate the
    // upload UX.
    return { kind: "files", paths };
  }

  // No real files on the clipboard. If the browser gave us text, insert
  // it at the caret — this is the dominant case when the user copies
  // agent output (mermaid / tables / code) and pastes it back.
  if (text) return { kind: "text", text };

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
  const imageFromDataTransfer = extractInlineImage(cd);
  if (imageFromDataTransfer) return { kind: "image", ...imageFromDataTransfer };
  const blob = await readClipboardImageBlob();
  if (blob) return { kind: "image", blob, extension: extensionForMime(blob.type) };

  // Last-resort: some WebViews populate `cd.files` even when
  // text/plain is empty and CF_HDROP is absent. Each File has only
  // `name` (no path), so we can't upload — the consumer inserts the
  // filename(s) as text for visual feedback.
  if (cd.files && cd.files.length > 0) {
    return { kind: "fileNames", names: Array.from(cd.files).map((f) => f.name) };
  }

  // Clipboard held neither paths nor files nor text nor an image.
  return { kind: "nothing" };
}

/**
 * Try to pull a raster image out of the paste event without an
 * async `clipboard.read()` roundtrip. Modern Chromium populates
 * `clipboardData.items` with `kind=file, type=image/*` for the
 * WebView's own copy-to-clipboard path; third-party screenshot
 * tools (Windows Snipping Tool, Snipaste, ShareX) typically do
 * NOT — they hand the OS a `CF_BITMAP` and the WebView sees no
 * DOM-level image item, only the async `ClipboardItem` surface.
 */
function extractInlineImage(
  cd: DataTransfer,
): { blob: Blob; extension: string } | null {
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
 * here — the synchronous `text` branch already handled that.
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
      "[pasteDetect] navigator.clipboard.read() failed (no image on clipboard, or permission denied)",
      err,
    );
    return null;
  }
}
