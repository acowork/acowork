// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Composer bridge — how a surface OUTSIDE the chat pane hands the user's
// current composer something to say.
//
// Why an event and not a store field: the composer draft is per-surface local
// state (`ChatPanel` keeps it in `useSessionScope`, `AgentChatView` in its own
// `useState`) precisely so a keystroke does not re-render the transcript. Only
// one of those two composers is mounted at a time (design 27 Wave 5: the tab
// strip shows one surface), so "the chat input box" is unambiguous on screen —
// which means the sender needs no session id, no agent id and no routing. Fire
// an event; the mounted composer applies it to its own session.
//
// This mirrors the two bridges the app already uses for the same reason
// (`showToast` / `openImageLightbox`, ADR-038): a plain module stays callable
// from a component, a store action or a future surface without threading
// context down.
//
// The bridge is transport-agnostic on purpose. The two composers upload in
// different ways — the Chat surface takes a `File` through
// `chatSessionStore.upload`, the execution surface writes a temp file through
// the Rust `save_clipboard_blob` command and then re-uses its path-based
// pipeline — so a file-carrying request carries the bytes (as a `File`) and
// each composer chooses its own way to the Gateway.
//
// `kind` says which END of the Gateway pipeline the file belongs to, not what
// it contains: "image" is a picture the model should see (a pasted screenshot,
// a cropped region — including a crop taken out of a PDF page), "file" is a
// document the model should be handed as itself (a whole PDF). The two differ
// downstream — the runtime rasterises the former into an image part and, for
// the latter, only offers a document part when the model can take a PDF — and
// the distinction cannot be recovered from the MIME type at the composer, which
// is why the sender states it.

/** One request for the composer currently on screen. */
export type ChatComposerRequest =
    | {
          kind: "text";
          /** Inserted at the caret, so the user can keep typing around it. */
          text: string;
      }
    | {
          kind: "image";
          /** Already named and MIME-typed — the composer uploads it as-is. */
          file: File;
      }
    | {
          kind: "file";
          /** A document attachment. Already named and MIME-typed. */
          file: File;
      };

const COMPOSER_EVENT = "acowork:chat-composer-request";

/** Insert `text` into the live composer (a no-op when none is mounted). */
export function insertIntoChatComposer(text: string): void {
    if (!text) return;
    dispatch({ kind: "text", text });
}

/** Attach `file` to the live composer's pending attachment tray, as a picture
 *  the model should see. */
export function attachImageToChatComposer(file: File): void {
    dispatch({ kind: "image", file });
}

/** Attach `file` to the live composer's pending attachment tray, as a document
 *  the model should receive whole (a PDF). */
export function attachFileToChatComposer(file: File): void {
    dispatch({ kind: "file", file });
}

/** Subscribe a composer to requests. Returns the unsubscribe function. */
export function subscribeChatComposerRequests(
    handler: (request: ChatComposerRequest) => void,
): () => void {
    const listener = (e: Event) => {
        const detail = (e as CustomEvent<ChatComposerRequest>).detail;
        if (!detail) return;
        if (detail.kind === "text" && !detail.text) return;
        if ((detail.kind === "image" || detail.kind === "file") && !detail.file) return;
        handler(detail);
    };
    window.addEventListener(COMPOSER_EVENT, listener);
    return () => window.removeEventListener(COMPOSER_EVENT, listener);
}

function dispatch(request: ChatComposerRequest): void {
    // `CustomEvent` detail is a same-document reference, so the `File` is not
    // copied — the composer receives the very bytes the caller produced.
    window.dispatchEvent(new CustomEvent<ChatComposerRequest>(COMPOSER_EVENT, { detail: request }));
}