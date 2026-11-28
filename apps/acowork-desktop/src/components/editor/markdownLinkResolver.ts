// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Cross-workspace link/image resolver for Markdown previews.
//
// Background:
//   Markdown files can carry relative hrefs/src that escape the file's own
//   workspace — e.g. `docs/zh/protocols/http.md` lives under one workspace
//   while its source-code link targets a different one. The naive
//   single-workspace resolver used to drop such links on the floor and the
//   webview's default navigation took over, leading to a navigation that
//   Tauri's `recover_from_wake` path could mistake for a system wake.
//
// What this module does:
//   1. `resolveLocalAssetPath` — pure path arithmetic against one workspace
//      root (reused by both link and image rendering).
//   2. `resolveAssetAcrossWorkspaces` — synchronous candidate list ordered
//      by priority (current workspace → `__agent_home__` → others).
//   3. `openFirstResolved` — async HEAD-probe over the candidate list and
//      call `openFile`/`openPreview` on the first workspace that contains
//      the file. Other candidates are silently skipped on 404.
//   4. `notifyLinkNotFound` — user-visible toast when nothing matches.
//
// Splitting the resolver from the React component keeps the click handler
// synchronous path arithmetic small and lets the React tree keep using the
// pure helper for image rendering without re-running the HEAD probe on
// every render.

import { defaultUrlTransform } from "react-markdown";
import { useAgentStore } from "../../stores/agentStore";
import { useChatStore } from "../../stores/chatStore";
import { useWorkspaceStore } from "../../stores/workspaceStore";
import { useFileEditorStore } from "../../stores/fileEditorStore";
import { getCachedWorkspaceRoot } from "../../stores/fileTree";
import { getGatewayUrl } from "../../lib/config";
import { log } from "../../lib/logger";
import { showToast } from "../common/ToastProvider";
import { pickOpenActionForPath } from "./filePreview";
import i18n from "../../i18n";
import {
    ABSOLUTE_PATH,
    PASSTHROUGH_SCHEMES,
    classifyMarkdownLink,
    docLinkRefs,
    parseChatLinkHref,
    resolveDocLink,
} from "../../lib/linkResolver";
import type { ChatLinkTarget } from "../../lib/linkResolver";

// The href grammar (scheme / anchor / `#L42` / drive-path normalisation) has one
// implementation, in `lib/linkResolver.ts`, because every markdown surface has to
// agree on it. Re-exported here so existing callers keep their import path.
export { ABSOLUTE_PATH, PASSTHROUGH_SCHEMES, classifyMarkdownLink, parseChatLinkHref };
export type { ChatLinkTarget };

/**
 * Resolve a markdown image/link `src` (relative or rooted) against one
 * workspace root and the directory of the markdown file. Handles `./` and
 * `../` segments. Returns a forward-slash separated absolute path suitable
 * for Tauri's `convertFileSrc` / `asset://` protocol, or `null` if `src`
 * is a non-file URL scheme or could not be resolved.
 *
 * Mirrors the legacy implementation that lived inside
 * `MarkdownPreviewView.tsx`; kept verbatim so existing callers and tests
 * still work.
 */
export function resolveLocalAssetPath(
    workspaceRoot: string,
    fileRelPath: string,
    src: string,
): string | null {
    if (!src || PASSTHROUGH_SCHEMES.test(src)) return null;
    if (ABSOLUTE_PATH.test(src)) return src;

    // Directory of the markdown file (its own relPath).
    const lastSep = Math.max(fileRelPath.lastIndexOf("/"), fileRelPath.lastIndexOf("\\"));
    const fileDir = lastSep >= 0 ? fileRelPath.substring(0, lastSep) : "";

    // Walk path segments, applying `./` and `../` against (workspaceRoot + fileDir).
    const baseParts = workspaceRoot.split(/[/\\]/).filter(Boolean);
    const fileDirParts = fileDir ? fileDir.split(/[/\\]/).filter(Boolean) : [];
    const srcParts = src.split(/[/\\]/);

    const result: string[] = [...baseParts];
    for (const part of [...fileDirParts, ...srcParts]) {
        if (part === "" || part === ".") continue;
        if (part === "..") {
            result.pop();
        } else {
            result.push(part);
        }
    }
    const joined = result.join("/");
    if (!joined) return null;

    // Re-attach the POSIX leading "/" if the workspace root was an
    // absolute path (macOS/Linux: starts with "/"). Without this,
    // split("/").filter(Boolean) above drops the empty first segment,
    // turning "/Users/foo/..." into "Users/foo/..." — a bare relative
    // path that Tauri's asset:// protocol cannot resolve (404).
    // Windows drive letters ("C:") survive filter(Boolean) intact, so
    // they don't need this treatment.
    if (workspaceRoot.startsWith("/") && !joined.startsWith("/")) {
        return `/${joined}`;
    }
    return joined;
}

/**
 * One candidate resolution of a markdown src — the absolute path (for
 * `convertFileSrc` rendering) plus the workspace-relative path (for
 * `openFile`/`openPreview`).
 */
export interface ResolvedAsset {
    workspaceId: string;
    /** Absolute filesystem path. Suitable for `convertFileSrc`. */
    absPath: string;
    /** Workspace-relative path. Suitable for `openFile(workspaceId, relPath)`. */
    relPath: string;
}

/**
 * Build the priority-ordered candidate list for resolving a markdown src
 * against all workspaces registered for the agent.
 *
 * Priority order:
 *   1. The current markdown file's workspace (`file.workspaceId`).
 *   2. `__agent_home__` (agent home directory).
 *   3. All other workspaces in the order returned by the store.
 *
 * Workspace root lookup prefers `getCachedWorkspaceRoot` (the path
 * returned by the workspace tree API for an already-loaded workspace)
 * and falls back to `workspaces[].path` (the canonical root returned by
 * the workspaces list API). Both should normally agree; the fallback
 * covers the case where the tree API has not yet been called.
 *
 * Returns an empty list for non-file schemes (http, data, mailto, tel,
 * asset, blob) and pure in-page anchors (`#…`). Returns a possibly-empty
 * list when the workspace list has not loaded yet — the caller should
 * decide whether to fall back to webview navigation in that case.
 */
export function resolveAssetAcrossWorkspaces(
    agentId: string,
    currentWorkspaceId: string,
    fileRelPath: string,
    src: string,
): ResolvedAsset[] {
    if (!src) return [];
    if (PASSTHROUGH_SCHEMES.test(src)) return [];
    if (src.startsWith("#")) return [];

    const { workspaces } = useWorkspaceStore.getState();

    const rootFor = (workspaceId: string): string | null => {
        const fromTree = getCachedWorkspaceRoot(agentId, workspaceId);
        if (fromTree) return fromTree;
        const ws = workspaces.find((w) => w.id === workspaceId);
        return ws?.path ?? null;
    };

    // Build priority-ordered (workspaceId, absRoot) list, deduplicated.
    const seen = new Set<string>();
    const ordered: Array<{ workspaceId: string; absRoot: string }> = [];
    const tryAdd = (workspaceId: string) => {
        if (!workspaceId || seen.has(workspaceId)) return;
        const absRoot = rootFor(workspaceId);
        if (!absRoot) return;
        seen.add(workspaceId);
        ordered.push({ workspaceId, absRoot });
    };
    tryAdd(currentWorkspaceId);
    tryAdd("__agent_home__");
    for (const ws of workspaces) tryAdd(ws.id);

    const out: ResolvedAsset[] = [];
    for (const { workspaceId, absRoot } of ordered) {
        const abs = resolveLocalAssetPath(absRoot, fileRelPath, src);
        if (!abs) continue;
        const rel = absPathToRelPath(absRoot, abs);
        if (rel === null) continue; // Absolute path fell outside this workspace root.
        out.push({ workspaceId, absPath: abs, relPath: rel });
    }
    return out;
}

/**
 * Convert an absolute path produced by `resolveLocalAssetPath` back to a
 * workspace-relative path suitable for `openFile`/`openPreview`.
 *
 * Returns `""` if the absolute path equals the workspace root, the
 * workspace-relative path with forward slashes if it lives under the root,
 * or `null` if the path is outside the workspace root (caller should
 * skip this candidate).
 */
function absPathToRelPath(workspaceRoot: string, absPath: string): string | null {
    const normRoot = workspaceRoot.replace(/\\/g, "/").replace(/\/+$/, "");
    const normAbs = absPath.replace(/\\/g, "/");
    if (normAbs === normRoot) return "";
    if (!normAbs.startsWith(normRoot + "/")) return null;
    return normAbs.slice(normRoot.length + 1);
}

/**
 * Build the Gateway URL for a workspace file read.
 *
 * Mirrors `buildFileUrl` inside `fileEditorStore.ts` but lives here so
 * this module has no private coupling to that store.
 */
function buildFileUrl(agentId: string, workspaceId: string, relPath: string): string {
    const baseUrl = getGatewayUrl();
    const params = new URLSearchParams();
    if (workspaceId && workspaceId !== "__agent_home__") {
        params.set("workspace_id", workspaceId);
    }
    params.set("path", relPath);
    return `${baseUrl}/api/agents/${agentId}/workspaces/file?${params.toString()}`;
}

/**
 * Send a lightweight HEAD probe to the Gateway to confirm the file exists
 * in the given workspace. We use HEAD rather than GET so the resolver
 * doesn't download the file body — the subsequent `openFile`/`openPreview`
 * will do the full GET.
 *
 * Axum auto-handles HEAD for any route registered with `get`, so the
 * existing `/workspaces/file` GET handler responds to HEAD with headers
 * (status + size) and no body.
 *
 * Returns `true` for any 2xx status, `false` for 404, 403, 5xx, and
 * network errors. The caller should treat 5xx as "exists, transient
 * failure" and proceed optimistically if desired, but here we err on the
 * side of falling through to the next candidate so users don't see
 * half-broken tabs.
 */
async function fileExists(agentId: string, workspaceId: string, relPath: string): Promise<boolean> {
    try {
        const url = buildFileUrl(agentId, workspaceId, relPath);
        const resp = await fetch(url, { method: "HEAD" });
        return resp.ok;
    } catch (e) {
        log.warn("[MarkdownLinkResolver] HEAD probe failed:", { workspaceId, relPath, err: String(e) });
        return false;
    }
}

/**
 * Outcome of an `openFirstResolved` attempt.
 *
 * - `opened: true` — one of the candidates exists and has been opened as
 *   a tab via `openFile`/`openPreview`. `workspaceId` tells the caller
 *   which workspace was selected (useful for telemetry / toast).
 * - `opened: false` — every candidate either resolved to a non-existent
 *   file or `candidates` was empty. Caller should surface a "not found"
 *   toast or otherwise notify the user.
 */
export type OpenResolutionResult =
    | { opened: true; workspaceId: string }
    | { opened: false; tried: number };

/**
 * Walk the candidate list in priority order, HEAD-probe each candidate,
 * and open the first one that exists. All 404s fall through silently.
 *
 * `action` chooses between edit mode (`openFile`) and read-only preview
 * mode (`openPreview`). The caller decides through `pickOpenActionForPath`
 * (the workspace-panel convention: a format with a viewer → preview, else
 * Monaco), and `MarkdownPreviewView.pickOpenAction` overrides it inside a
 * rendered markdown document.
 *
 * Concurrent calls are intentionally allowed: each call HEAD-probes
 * independently and the `openFile`/`openPreview` actions are idempotent
 * (they just activate the existing tab if one is already open for the
 * same `agentId:workspaceId:relPath`).
 */
export async function openFirstResolved(
    agentId: string,
    candidates: ResolvedAsset[],
    action: "openFile" | "openPreview",
    /** Optional 1-based line number to reveal after opening (edit mode only).
     *  Forwarded to `openFile(..., line)` which sets `cursorLine` on the
     *  open file; `FileEditorPanel` then `revealLineInCenter(line)` +
     *  `setPosition({ lineNumber: line, column: 1 })`. Ignored when
     *  `action === "openPreview"`. ADR-046 § follow-up: extending this to
     *  carry an `endLine` selection range is deferred to a future change. */
    line?: number,
): Promise<OpenResolutionResult> {
    if (candidates.length === 0) return { opened: false, tried: 0 };

    const store = useFileEditorStore.getState();
    for (const c of candidates) {
        const exists = await fileExists(agentId, c.workspaceId, c.relPath);
        if (!exists) continue;
        if (action === "openFile") {
            await store.openFile(agentId, c.workspaceId, c.relPath, line);
        } else {
            await store.openPreview(agentId, c.workspaceId, c.relPath);
        }
        return { opened: true, workspaceId: c.workspaceId };
    }
    return { opened: false, tried: candidates.length };
}

/**
 * Fire a toast informing the user that the clicked link/image target
 * could not be found — in any workspace **and** in the doc library.
 *
 * The two stages are one answer on purpose: the user clicked a link that names
 * nothing reachable, and telling them "not in a workspace" when the library was
 * also searched would send them looking in the wrong place.
 */
export function notifyLinkNotFound(href: string): void {
    showToast({
        type: "warning",
        message: i18n.t("fileEditor.markdownLinkNotFound", { href }),
    });
}

/**
 * Fire the permission toast for a link that lands on a doc-library entry the
 * caller may not view.
 *
 * A separate message from "not found" because they are different facts: the
 * entry exists, the link is correct, and the fix is to ask for access — not to
 * hunt for a typo.
 */
export function notifyLinkDenied(href: string): void {
    showToast({
        type: "warning",
        message: i18n.t("fileEditor.markdownLinkDenied", { href }),
    });
}

/**
 * Second resolution stage for any markdown link: the doc library.
 *
 * Runs **after** the workspace stage finds nothing (documents are delivered
 * into the library, so a workspace file of the same name wins and the tab keeps
 * its editing abilities). Outcome handling is here rather than at each call
 * site so chat bubbles and the preview cannot drift apart on wording:
 *
 *   - `found`  → `openDocEntry`, which picks the text view or the byte viewer
 *                by extension (a `.pdf` / `.pptx` opens as itself);
 *   - `denied` → permission toast (the entry exists);
 *   - anything else → "not found" toast (no fuzzy matching, no searching).
 */
export async function openDocLinkOrNotify(
    href: string,
    opts: { basePath?: string } = {},
): Promise<void> {
    const refs = opts.basePath ? docLinkRefs(opts.basePath, href) : docLinkRefs("", href);
    let denied = false;
    for (const ref of refs) {
        const outcome = await resolveDocLink(ref);
        if (outcome.status === "found") {
            useFileEditorStore.getState().openDocEntry(outcome.entry);
            return;
        }
        if (outcome.status === "denied") denied = true;
    }
    // 权限优先于「不存在」：条目确实在，文案应当指向「去申请权限」而不是让人
    // 去找一个打错方向的错字。
    if (denied) {
        notifyLinkDenied(href);
        return;
    }
    notifyLinkNotFound(href);
}

// ── Chat-surface markdown link handling ────────────────────────────────
//
// `[text](D:/repo/file.md)` — a Windows drive-path link, the shape agents
// produce for file references. react-markdown's stock `defaultUrlTransform`
// treats `D:` as an unknown protocol and rewrites the href to `""`; the
// resulting `<a href="">` then default-navigates the webview to the current
// URL on click — a full page reload, and on WebView2 the historical
// renderer crash that killed the whole app. The exports below are the
// chat-side fix: `markdownUrlTransform` keeps local paths alive through
// sanitisation, `parseChatLinkHref` normalises the clickable shapes, and
// `handleChatMarkdownLinkClick` resolves them against the agent's
// workspaces exactly like the attachment chips do.

/**
 * react-markdown `urlTransform` for every chat-side markdown surface.
 *
 * Differences from the stock `defaultUrlTransform` (react-markdown 10.x):
 *   - Windows drive paths (`D:/…`, `D:\…`) pass through unchanged. The
 *     stock transform classifies `D:` as an unknown protocol and returns
 *     `""`, destroying the link. The click handler resolves the drive
 *     path against the agent's workspace roots.
 *   - `file:///…` URLs are converted to plain local absolute paths
 *     (`file:///D:/repo/a.md` → `D:/repo/a.md`, `file:///home/u/a.md` →
 *     `/home/u/a.md`) for the same reason — the stock transform strips
 *     them because `file:` is not an allow-listed protocol.
 *
 * Everything else keeps the stock sanitisation (`javascript:` etc. are
 * still rewritten to `""`), so the XSS posture is unchanged.
 */
export function markdownUrlTransform(url: string): string {
    // `D:/…` / `D:\…` — Windows drive path.
    if (/^[A-Za-z]:[/\\]/.test(url)) return url;
    // `file:///…` — convert to a local path; fall back to the stock
    // sanitisation when the URL is malformed or is a UNC path
    // (`file://server/share`), which the workspace resolver cannot map.
    if (/^file:\/\//i.test(url)) {
        const local = fileUrlToLocalPath(url);
        if (local) return local;
    }
    return defaultUrlTransform(url);
}

/** `file:///D:/x` → `D:/x`; `file:///home/u/x` → `/home/u/x`; UNC → null. */
function fileUrlToLocalPath(url: string): string | null {
    try {
        const u = new URL(url);
        if (u.host) return null; // UNC (`file://server/…`) — not resolvable.
        let p = decodeURIComponent(u.pathname);
        if (/^\/[A-Za-z]:/.test(p)) p = p.slice(1); // `/D:/…` → `D:/…`
        return p;
    } catch {
        return null;
    }
}

/**
 * Click dispatch for markdown links inside chat surfaces (MessageBubble,
 * CompactionCard). Chat links have no file-of-origin context, so local
 * paths are resolved against the agent's workspaces — the same
 * candidate-walk the attachment chips use (`resolveAssetAcrossWorkspaces`
 * + `openFirstResolved`), which transparently upgrades drive-absolute
 * paths (`D:/repo/docs/a.md`) to a workspace-relative open.
 *
 * http(s) links open in the URL preview tab. Local paths open in Monaco
 * (images in preview mode); a link that matches no workspace file raises
 * the standard "not found" toast. In-page anchors are silently ignored.
 * Callers MUST have already called `preventDefault()` — this function
 * never lets the webview navigate.
 */
export function handleChatMarkdownLinkClick(href: string): void {
    // One classification for every markdown surface (`lib/linkResolver.ts`):
    // anchors and pass-through schemes are not ours to handle.
    const link = classifyMarkdownLink(href);
    if (link.kind === "anchor" || link.kind === "passthrough") return;

    const agentId = useAgentStore.getState().selectedAgentId;
    if (link.kind === "external") {
        if (agentId) useFileEditorStore.getState().openUrl(agentId, link.url ?? href);
        return;
    }

    // The workspace stage needs an agent and an active session; the doc-library
    // stage needs neither (the doc proxy is Gateway-wide). So "no agent" is no
    // longer a dead end — the click just skips straight to the library.
    const path = link.path ?? href;
    const sessionId = agentId ? useChatStore.getState().getActiveSessionId(agentId) : null;
    const workspaceId = sessionId
        ? useWorkspaceStore.getState().getSessionWorkspaceId(sessionId)
        : "";

    if (agentId && sessionId) {
        // `#L42` on a link means "show me that line", and the preview viewer has
        // no line jump — so an anchored link stays on the source editor even for
        // a format that would otherwise preview.
        const line = link.line;
        const action = line != null ? "openFile" : pickOpenActionForPath(path);
        let candidates = resolveAssetAcrossWorkspaces(agentId, workspaceId, "", path);
        if (candidates.length === 0 && path.startsWith("/")) {
            // Legacy chat convention: a bare leading slash means
            // "workspace-relative", not "POSIX absolute".
            candidates = resolveAssetAcrossWorkspaces(agentId, workspaceId, "", path.slice(1));
        }
        if (candidates.length > 0) {
            void openFirstResolved(agentId, candidates, action, line).then((result) => {
                // A workspace miss is not the end of the road: the link may name
                // a doc-library entry, which is the shape agents actually produce
                // for their deliverables.
                if (!result.opened) void openDocLinkOrNotify(path);
            });
            return;
        }
    }
    void openDocLinkOrNotify(path);
}