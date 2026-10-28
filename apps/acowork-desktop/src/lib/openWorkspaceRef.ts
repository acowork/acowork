// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Single dispatch entry point for "open this workspace attachment in the
// in-app fileTab" — used by the chat-bubble and input-bar chips created
// from "Add to Chat" references.
//
// Why this exists:
//   - `MessageBubble.tsx` and `AttachedContextChips.tsx` both render three
//     workspace-ref variants (`attached_file` / `attached_selection` /
//     `attached_folder`) as clickable chips. Their click handlers should
//     converge on **one** piece of logic that knows how to map an absolute
//     filesystem path to a `(workspaceId, relPath)` pair, pick the right
//     fileTab action, and report failures uniformly.
//   - `markdownLinkResolver.ts` already owns the path → workspace mapping
//     (`resolveAssetAcrossWorkspaces` + `openFirstResolved`) and the
//     "not found" toast (`notifyLinkNotFound`). We reuse those — no
//     parallel machinery, no duplicated HEAD-probe code.
//
// Three entry shapes are accepted so callers don't have to normalise first:
//
//   | ChatStore `attachedContext` shape (`type: "file" | "directory" | "selection"`)
//   | Wire `AttachedItem` shape     (`type: "attached_file" | "attached_folder" | "attached_selection"`)
//
// `file_upload` / `image_upload` are **not** accepted here on purpose:
// this dispatcher's contract is an `absPath` resolved across workspaces, and a
// chat attachment has none — its bytes live in the user service
// (`chat-sessions/{sid}/files/`). Uploaded files are therefore clickable, but
// through a different dispatcher: `lib/openChatAttachment.ts` (ADR-046 §2.5
// omits uploads from this table, it does not forbid opening them). The
// execution surface's own uploads stay inert by ADR-100 D12 — those bytes
// belong to that run, not to this chat.
//
// Action selection (preview vs. Monaco edit) is delegated to the shared
// `pickOpenActionForPath` helper so this dispatcher stays in lock-step
// with the workspace tree and the chat message `<a>` interceptor.

import {
    notifyLinkNotFound,
    openFirstResolved,
    pickOpenActionForPath,
    resolveAssetAcrossWorkspaces,
} from "../components/editor/markdownLinkResolver";
import { log } from "./logger";
import type { AttachedItem } from "./types";

/** Chat-store "pending attachment" shape — the pre-send form of a workspace
 *  ref. `startLine`/`endLine` are 1-based and inclusive for selection. */
export interface AttachedContextLike {
    type: "file" | "directory" | "selection";
    absPath: string;
    name: string;
    startLine?: number;
    endLine?: number;
}

/** Union of every workspace-ref shape this dispatcher accepts. */
export type WorkspaceRefItem = AttachedItem | AttachedContextLike;

interface OpenAttachedRefArgs {
    /** The chip item. */
    item: WorkspaceRefItem;
    /** Selected agent ID. The dispatch is a no-op when this is falsy. */
    agentId: string | null;
    /** Workspace ID for the current session — used as the highest-priority
     *  candidate root in `resolveAssetAcrossWorkspaces`. */
    currentWorkspaceId: string;
}

/**
 * Open a workspace attachment in the in-app fileTab.
 *
 * Behaviour by `item.type`:
 *   - `file` / `attached_file`         → action picked by
 *       `pickOpenActionForPath(absPath)` — images go to `openPreview`,
 *       everything else (including JSON, Markdown, HTML, source code) goes
 *       to `openFile` so Monaco renders the source rather than the markdown
 *       preview fallback (which used to freeze on multi-MB JSON files).
 *   - `selection` / `attached_selection` → `openFile(..., startLine)`. The
 *       cursor jumps to `startLine`; multi-line selection highlight is
 *       intentionally **not** implemented yet (deferred — needs
 *       `selectionRange` on `fileEditorStore`). Selections always open in
 *       Monaco (never preview) — the user is pointing at a specific line.
 *   - `directory` / `attached_folder`  → silent no-op (fileTab doesn't open
 *       folders yet; waiting for folder-preview support).
 *
 * Returns a `Promise<void>` so callers can `await` and surface errors if
 * desired; current behaviour swallows all errors and emits a toast on
 * "not found in any workspace" via `notifyLinkNotFound`.
 */
export async function openAttachedRef({
    item,
    agentId,
    currentWorkspaceId,
}: OpenAttachedRefArgs): Promise<void> {
    if (!agentId) return;

    // ── Branch on the user's "what does a click mean" intent ──────────
    switch (item.type) {
        case "file":
        case "attached_file": {
            // Match the workspace-panel convention: images preview, every
            // other supported text/source format (including JSON, MD, HTML,
            // and source code) opens in Monaco. Previously this always went
            // through `openPreview`, which falls through to
            // `MarkdownPreviewView` for non-image / non-HTML files and
            // freezes on multi-MB JSON.
            const action = pickOpenActionForPath(item.absPath);
            await openWithAction(action, item.absPath, agentId, currentWorkspaceId);
            return;
        }
        case "selection":
        case "attached_selection": {
            // 1-based, inclusive. Fall back to 1 if upstream forgot to set them.
            const startLine = item.startLine ?? 1;
            await openAtLine(item.absPath, agentId, currentWorkspaceId, startLine);
            return;
        }
        case "directory":
        case "attached_folder":
            // fileTab doesn't support folder preview yet — silent no-op per
            // product direction. (Future: navigate the workspace tree to
            // the folder instead of opening a tab.)
            return;
        case "file_upload":
        case "image_upload":
            // ADR-046: uploads are not clickable workspace refs. Defensive
            // branch — callers shouldn't reach here.
            log.warn("[openAttachedRef] upload items are not clickable:", item);
            return;
        default: {
            // Exhaustiveness guard.
            const _exhaustive: never = item;
            log.warn("[openAttachedRef] unknown item type:", _exhaustive);
            return;
        }
    }
}

// ── Internals ─────────────────────────────────────────────────────────

async function openWithAction(
    action: "openFile" | "openPreview",
    absPath: string,
    agentId: string,
    currentWorkspaceId: string,
): Promise<void> {
    const candidates = resolveAssetAcrossWorkspaces(agentId, currentWorkspaceId, "", absPath);
    if (candidates.length === 0) {
        notifyLinkNotFound(absPath);
        return;
    }
    const result = await openFirstResolved(agentId, candidates, action);
    if (!result.opened) {
        notifyLinkNotFound(absPath);
    }
}

async function openAtLine(
    absPath: string,
    agentId: string,
    currentWorkspaceId: string,
    startLine: number,
): Promise<void> {
    const candidates = resolveAssetAcrossWorkspaces(agentId, currentWorkspaceId, "", absPath);
    if (candidates.length === 0) {
        notifyLinkNotFound(absPath);
        return;
    }
    const result = await openFirstResolved(agentId, candidates, "openFile", startLine);
    if (!result.opened) {
        notifyLinkNotFound(absPath);
    }
}