/**
 * Save a Gateway-served file to disk.
 *
 * The transfer happens in Rust (`download_attachment`), not here — the bytes
 * must never travel over the Tauri IPC as an `invoke` argument, because a
 * named `Uint8Array` is serialized into a JSON array of decimal numbers. That
 * froze the WebView main thread for seconds on a multi-MB attachment and left
 * the `promise` unsettled (the button spun forever). Only the URL and the
 * chosen path cross the bridge; Rust fetches with the account token and writes
 * the file.
 *
 * Why this is its own module: the transfer is one rule with two address spaces
 * behind it — chat-session blobs (`/api/chat-sessions/…`) and agent files
 * (`/api/agents/…/files/…`). Before it was extracted, the chat copy owned the
 * only implementation and the agent side had to re-invent it, which is how a
 * second, weaker save path gets written.
 *
 * Returns `false` when the user cancelled the dialog (not an error).
 *
 * `onProgress` receives the byte count already written to disk, pushed from
 * Rust over an IPC channel — the total comes from the caller's own size
 * metadata, so no extra round trip is needed for a bar.
 */
import { Channel, invoke } from "@tauri-apps/api/core";
import { save as showSaveDialog } from "@tauri-apps/plugin-dialog";

export async function saveUrlToDisk(
  url: string,
  filename: string,
  onProgress?: (received: number) => void,
): Promise<boolean> {
  const picked = await showSaveDialog({
    defaultPath: filename,
    filters: [{ name: filename, extensions: [extensionOf(filename)] }],
  });
  if (!picked) return false;
  const progress = new Channel<number>();
  if (onProgress) progress.onmessage = onProgress;
  await invoke("download_attachment", { url, path: picked, onProgress: progress });
  return true;
}

/** Extension without the dot, lowercased — used to seed the save
 *  dialog's filter so the picker preselects the right file type. */
function extensionOf(filename: string): string {
  const dot = filename.lastIndexOf(".");
  return dot >= 0 ? filename.slice(dot + 1).toLowerCase() : "";
}
