/**
 * User↔user chat HTTP client (ADR-076 §决策 8).
 *
 * Plain `fetch`, **no token plumbing**: these paths are not under
 * `/api/auth/*`, so the app-wide interceptor ([authFetch.ts](authFetch.ts))
 * attaches the bearer and handles the 401 → refresh → replay for us.
 * Threading tokens through here would be a second, weaker copy of that.
 *
 * Every route below exists only under `AUTH_MODE=multi_user`
 * (ADR-076 §决策 12); under `local` the view that uses them is never
 * mounted.
 *
 * One exception to the plain-`fetch` rule: attachment *downloads* go
 * through the Rust command in `downloadChatAttachment` (the account
 * token comes from the Desktop's mirrored `GatewayAuth`, not this
 * module). See that function for why.
 */

import { readError } from "./auth-api";
import { getGatewayUrl } from "./config";
import { isTextReadablePath } from "./monacoLanguage";
import { Channel, invoke } from "@tauri-apps/api/core";
import { save as showSaveDialog } from "@tauri-apps/plugin-dialog";
import type {
  ChatAttachment,
  UserChatMessage,
  UserChatMessagesPage,
  UserChatSummary,
  UserDirectoryEntry,
} from "./types";

/** Mirrors the Gateway's `DEFAULT_PAGE`. */
export const CHAT_PAGE_SIZE = 50;

function base(userId: string, chatId?: string): string {
  const root = `${getGatewayUrl()}/api/users/${encodeURIComponent(userId)}/chats`;
  return chatId ? `${root}/${encodeURIComponent(chatId)}` : root;
}

async function decode<T>(resp: Response): Promise<T> {
  if (!resp.ok) throw new Error(await readError(resp));
  return (await resp.json()) as T;
}

/** Conversations for `userId`, most recently active first. */
export async function listChats(userId: string): Promise<UserChatSummary[]> {
  const data = await decode<{ chats?: UserChatSummary[] }>(
    await fetch(base(userId)),
  );
  // A 200 whose body is not the expected shape must not write `undefined`
  // into the store — `decode` only checks the status, and the type is a
  // compile-time claim the wire does not owe us.
  return Array.isArray(data.chats) ? data.chats : [];
}

/**
 * One page of history, returned oldest-first within the page. `offset`
 * counts **back** from the newest message (0 = latest page).
 */
export async function listMessages(
  userId: string,
  chatId: string,
  offset = 0,
  limit = CHAT_PAGE_SIZE,
): Promise<UserChatMessagesPage> {
  const query = `?offset=${offset}&limit=${limit}`;
  const page = await decode<UserChatMessagesPage>(
    await fetch(`${base(userId, chatId)}/messages${query}`),
  );
  return { ...page, messages: Array.isArray(page.messages) ? page.messages : [] };
}

/** Send as `userId`. The Gateway takes `from` from the token, not from us. */
export async function sendChatMessage(
  userId: string,
  chatId: string,
  body: string,
  attachmentIds: string[] = [],
): Promise<UserChatMessage> {
  return decode<UserChatMessage>(
    await fetch(`${base(userId, chatId)}/messages`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({ body, attachments: attachmentIds }),
    }),
  );
}

/**
 * Upload one file and get back the id the message will reference.
 *
 * `FormData` sets its own multipart boundary, so no `Content-Type` header
 * here — setting one by hand is what breaks uploads.
 */
export async function uploadChatAttachment(
  userId: string,
  chatId: string,
  file: File,
): Promise<ChatAttachment> {
  const form = new FormData();
  form.append("file", file, file.name);
  return decode<ChatAttachment>(
    await fetch(`${base(userId, chatId)}/files`, { method: "POST", body: form }),
  );
}

/**
 * Blob URLs for attachments, keyed by id.
 *
 * Downloads need the bearer token, so `<img src="/api/...">` cannot work —
 * the global interceptor only sees `fetch`. Fetch the bytes and hand the
 * element an object URL instead; that also keeps the token out of any URL
 * that could be logged or shared.
 *
 * ponytail: entries are never revoked. A desktop session sees a bounded
 * number of attachments, and each blob dies with the page anyway; add an
 * eviction pass if a long-lived session ever accumulates them.
 */
const blobUrls = new Map<string, Promise<string>>();

/** In-flight uploads are already deduped by id, so one URL per attachment. */
export function attachmentObjectUrl(
  userId: string,
  chatId: string,
  attachmentId: string,
): Promise<string> {
  let url = blobUrls.get(attachmentId);
  if (!url) {
    url = (async () => {
      const resp = await fetch(
        `${base(userId, chatId)}/files/${encodeURIComponent(attachmentId)}`,
      );
      if (!resp.ok) throw new Error(await readError(resp));
      return URL.createObjectURL(await resp.blob());
    })();
    // A failed fetch must not be cached as a permanent failure.
    url.catch(() => blobUrls.delete(attachmentId));
    blobUrls.set(attachmentId, url);
  }
  return url;
}

/**
 * Object URL for an attachment's **thumbnail** sidecar, or `null` when it has
 * none.
 *
 * The same `?thumb=1` route the agent-side preview uses, so a chat row costs
 * a few KB instead of the whole blob. This is the path `ChatAttachment.thumb`
 * exists for: a `null` here means "no preview was generated at upload" and
 * the caller falls back to the original — it is not an error worth surfacing.
 *
 * Failures resolve to `null` rather than rejecting, for the same reason the
 * inline fetch above is separate: a missing thumbnail is a cosmetic gap, and
 * a rejected promise in a render effect is an unhandled rejection.
 */
export async function attachmentThumbObjectUrl(
  userId: string,
  chatId: string,
  attachment: ChatAttachment,
): Promise<string | null> {
  if (!attachment.thumb) return null;
  try {
    const resp = await fetch(
      `${base(userId, chatId)}/files/${encodeURIComponent(attachment.id)}?thumb=1`,
    );
    if (!resp.ok) return null;
    const blob = await resp.blob();
    // A 200 with no body is a broken thumbnail, not a thumbnail: handing an
    // empty blob to `<img src>` draws a broken-image glyph that reads as a
    // decode failure rather than as "no preview here".
    if (blob.size === 0) return null;
    return URL.createObjectURL(blob);
  } catch {
    return null;
  }
}

/**
 * The opening lines a `txt` thumbnail sidecar holds, captured at upload.
 *
 * Separate from [`attachmentThumbObjectUrl`] because a text sidecar is
 * rendered as text, not handed to `<img>`: an object URL would send the bytes
 * through a second fetch just to read them back. `null` when there is no
 * sidecar or it cannot be read.
 */
export async function attachmentThumbText(
  userId: string,
  chatId: string,
  attachment: ChatAttachment,
): Promise<string | null> {
  if (attachment.thumb !== "txt") return null;
  try {
    const resp = await fetch(
      `${base(userId, chatId)}/files/${encodeURIComponent(attachment.id)}?thumb=1`,
    );
    if (!resp.ok) return null;
    const text = await resp.text();
    return text.trim() ? text : null;
  } catch {
    return null;
  }
}

/** Download an attachment to disk: OS save dialog, then the transfer.
 *
 *  The transfer happens in Rust (`download_attachment`), not here — the
 *  bytes must never travel over the Tauri IPC as an `invoke` argument,
 *  because a named `Uint8Array` is serialized into a JSON array of
 *  decimal numbers. That froze the WebView main thread for seconds on a
 *  multi-MB attachment and left the `invoke` promise unsettled (the
 *  button spun forever). Only the URL and the chosen path cross the
 *  bridge; Rust fetches with the account token and writes the file.
 *
 *  Returns `false` when the user cancelled the dialog (not an error).
 *
 *  `onProgress` receives the byte count already written to disk, pushed
 *  from Rust over an IPC channel — the total comes from
 *  `attachment.size`, so no extra round trip is needed for a bar. */
export async function downloadChatAttachment(
  userId: string,
  chatId: string,
  attachment: ChatAttachment,
  onProgress?: (received: number) => void,
): Promise<boolean> {
  const picked = await showSaveDialog({
    defaultPath: attachment.filename,
    filters: [{ name: attachment.filename, extensions: [extensionOf(attachment.filename)] }],
  });
  if (!picked) return false;
  const progress = new Channel<number>();
  if (onProgress) progress.onmessage = onProgress;
  await invoke("download_attachment", {
    url: `${base(userId, chatId)}/files/${encodeURIComponent(attachment.id)}`,
    path: picked,
    onProgress: progress,
  });
  return true;
}

/** Extension without the dot, lowercased — used to seed the save
 *  dialog's filter so the picker preselects the right file type. */
function extensionOf(filename: string): string {
  const dot = filename.lastIndexOf(".");
  return dot >= 0 ? filename.slice(dot + 1).toLowerCase() : "";
}

/**
 * Largest attachment we will load into Monaco for an in-app preview.
 *
 * A preview puts the whole body in a JS string and hands it to Monaco to
 * tokenize on the main thread, so unlike the download path this is *not*
 * size-independent. 8 MiB covers source files, configs and most logs while
 * staying well inside the "no visible freeze" range; anything larger keeps
 * the download-only affordance.
 */
export const PREVIEW_MAX_BYTES = 8 * 1024 * 1024;

/**
 * Whether an attachment can be previewed in-app (Monaco tab) instead of
 * only downloaded: a text-readable extension within `PREVIEW_MAX_BYTES`.
 *
 * `size` is the upload-time metadata, so a file whose record is wrong may
 * still be refused (or accepted) on the boundary — harmless either way,
 * the preview just has to survive what it is given.
 */
export function canPreviewAttachment(attachment: ChatAttachment): boolean {
  return attachment.size <= PREVIEW_MAX_BYTES && isTextReadablePath(attachment.filename);
}

/**
 * Fetch one attachment as text, for the read-only preview tab.
 *
 * Goes through the same global fetch interceptor as every other call in
 * this module (the bearer token is attached there), not through
 * `attachmentObjectUrl` — that helper exists for `<img>` and its entries
 * are never revoked, which is the wrong trade for a one-shot text read.
 *
 * The body is *not* size-checked here: callers gate on
 * [`canPreviewAttachment`], and a caller that skips that gate gets whatever
 * the Gateway serves.
 */
export async function fetchAttachmentText(
  userId: string,
  chatId: string,
  attachment: ChatAttachment,
): Promise<string> {
  const resp = await fetch(`${base(userId, chatId)}/files/${encodeURIComponent(attachment.id)}`);
  if (!resp.ok) throw new Error(await readError(resp));
  return resp.text();
}

/** Human-readable size, matching how the file pickers show one. */
export function formatAttachmentSize(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value < 10 ? value.toFixed(1) : Math.round(value)} ${units[unit]}`;
}

/** Clear this user's unread counter for the conversation. */
export async function markChatRead(
  userId: string,
  chatId: string,
): Promise<void> {
  const resp = await fetch(`${base(userId, chatId)}/read`, { method: "POST" });
  if (!resp.ok) throw new Error(await readError(resp));
}

/**
 * Contacts for the "new conversation" picker: every enabled account except
 * the caller.
 *
 * The route lives in `account_api` but exists for this view — it is the one
 * account listing a non-admin may read (ADR-076 §决策 8), because a chat
 * feature with no way to name a recipient is unusable for everyone who is
 * not an admin. The Gateway owns what is exposed; this is a plain read.
 */
export async function listUserDirectory(): Promise<UserDirectoryEntry[]> {
  const data = await decode<{ users: UserDirectoryEntry[] }>(
    await fetch(`${getGatewayUrl()}/api/users/directory`),
  );
  return data.users;
}

/** `display_name` if set, else the login handle — never blank. */
export function contactLabel(entry: UserDirectoryEntry): string {
  return entry.display_name || entry.username;
}
