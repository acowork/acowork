// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Workspace-panel "is this file previewable?" rule, and the hosted-app
// extension table every preview decision reads (ADR-121).
//
// Centralised so the workspace tree (double-click, search select, right-click
// menu), the editor panel (preview/source toggle, viewer choice) and the file
// store (which files carry no text content) can't drift out of sync. The
// single source of truth replaces four near-duplicate regexes that were
// previously scattered across:
//
//   - WorkspaceExplorer.handleFileDoubleClick  (file-tree double-click)
//   - WorkspaceExplorer.handleSearchSelect     (Ctrl+F result pick)
//   - FileTreeNode.isPreviewable               (right-click "Preview" item)
//   - FileEditorPanel.PREVIEW_TOGGLE_RE        (editor preview toggle)
//
// Rule (workspace-panel convention):
//   - Raster images (jpg / jpeg / png / gif / webp) → image viewer.
//   - SVG, Markdown, HTML / HTM                    → previewable renderer.
//   - CSV                                           → sheet viewer / text.
//   - A hosted craft app claims the file when it is installed and enabled
//     and declared the extension in `ui.canvas.extensions` (ADR-121): the
//     preview is that app's iframe in the editor pane, never a chat bubble.
//     Office documents (docx / xlsx / pptx / pdf) keep their built-in
//     viewers until an office craft app is enabled; camera raw, HEIF/JXL/
//     PSD photos and vector interchange files (ai / eps / dxf / …) have no
//     built-in viewer, so a craft app is their only in-app renderer.
//   - Everything else (JSON, source code, configs) → Monaco source editor.
//
// NB: two rules live here, and they are intentionally different. The table
// below is the **workspace-panel** rule; `pickOpenActionForPath` at the foot of
// this file is the **chat-side** rule (images only), because a chat click on a
// `.md` file expects source, not a re-render. Don't unify them — and don't move
// either one into a consumer: deciding how a file opens must not require the
// markdown renderer, the toast system or any store.

/** Native craft documents: the app iframe renders them, no built-in viewer. */
export const NATIVE_CRAFT_EXTS: readonly string[] = [
    "figcraft",
    "deckcraft",
    "vectorcraft",
    "drawcraft",
    "vctemplate",
    "scjson",
];

/**
 * Formats with no built-in Desktop viewer, so an enabled hosted app is the
 * only in-app renderer:
 *   - camera raw + containers no webview decodes (LightCraft),
 *   - vector interchange and metafiles (VectorCraft).
 */
export const HOSTED_ONLY_EXTS: readonly string[] = [
    // Camera raw — the list is LightCraft's import set (crates/engine/src/import/files.rs).
    "3fr",
    "arw",
    "cr2",
    "cr3",
    "crw",
    "dng",
    "erf",
    "fff",
    "iiq",
    "kdc",
    "mef",
    "mos",
    "mrw",
    "nef",
    "nrw",
    "orf",
    "pef",
    "raf",
    "raw",
    "rw2",
    "rwl",
    "sr2",
    "srf",
    "srw",
    "x3f",
    // Photos a webview cannot decode: HEIF, JPEG XL, Photoshop.
    "heic",
    "heif",
    "jxl",
    "psb",
    "psd",
    "tif",
    "tiff",
    // Vector interchange + metafiles VectorCraft reads.
    "ai",
    "ait",
    "cdr",
    "cdt",
    "dxf",
    "emf",
    "eps",
    "epsf",
    "hpgl",
    "plt",
    "svgz",
    "wmf",
];

/** Office / PDF documents: built-in viewers exist, a craft app may take them over. */
export const OFFICE_EXTS: readonly string[] = [
    "pdf",
    "docx",
    "doc",
    "xlsx",
    "xls",
    "pptx",
    "ppt",
];

/** `\.(?:a|b)$` for a list of bare extensions. */
export function previewExtRe(exts: readonly string[]): RegExp {
    return new RegExp(`\\.(?:${exts.join("|")})$`, "i");
}

/**
 * Everything a hosted app may be asked to render — and the set whose tab
 * carries NO text content (the app fetches the bytes itself). Formats with a
 * built-in viewer (png / jpg / svg / md / csv) are absent on purpose: a craft
 * app still takes them over when it declared them, but the store keeps the
 * read path those viewers use.
 */
export const HOSTED_PREVIEW_EXT_RE = previewExtRe([
    ...NATIVE_CRAFT_EXTS,
    ...HOSTED_ONLY_EXTS,
    ...OFFICE_EXTS,
]);

/** Extensions that the workspace panel treats as previewable. */
export const WORKSPACE_PREVIEW_EXT_RE = previewExtRe([
    "jpg",
    "jpeg",
    "png",
    "gif",
    "webp",
    "svg",
    "md",
    "html?",
    "csv",
    ...NATIVE_CRAFT_EXTS,
    ...HOSTED_ONLY_EXTS,
    ...OFFICE_EXTS,
]);

/**
 * Decide whether `name` (a basename or relative path) should open in preview
 * mode from a workspace-panel entry point. Both forward and backward
 * slashes are tolerated because the file tree sometimes hands us a
 * workspace-relative POSIX path even on Windows.
 */
export function isWorkspacePreviewable(name: string): boolean {
    return WORKSPACE_PREVIEW_EXT_RE.test(name);
}

/**
 * Chat-side companion rule: a file opens in the preview viewer when the
 * workspace panel would preview it — images, PDF/Office documents, and the
 * craft-app files — **except** for the text formats where a chat click means
 * "show me the source".
 *
 * `*.md` / `*.mdx` are the exception on purpose: a Markdown link inside a
 * message sits in already-rendered prose, so re-rendering it is a dead end and
 * editing it is the useful action. The workspace tree has the opposite
 * default because its whole job is browsing documents.
 *
 * Everything else — JSON, HTML, CSV, source code — goes to Monaco on both
 * surfaces: it streams multi-MB files, whereas routing them through
 * `MarkdownPreviewView` used to freeze the page. Binary formats are NOT in
 * that set: reading one through the text path returns base64 (the read
 * interface has no binary channel), which is exactly why a click must never
 * fall back to `openFile` for them.
 *
 * This is the single rule for "the user clicked a file reference in chat",
 * shared by:
 *   - `lib/openWorkspaceRef.openAttachedRef` — the ref chip in the input bar
 *     and in a message bubble;
 *   - the `<a>` interceptor in `markdownLinkResolver` — a markdown link in a
 *     message;
 *   - the turn deliverables row (ADR-119) — a thumbnail or file chip.
 *
 * NB: `MarkdownPreviewView`'s internal `pickOpenAction` is a *third* rule
 * (`.md → preview`, else edit) and stays local to that component: inside a
 * rendered markdown document, a `.md` link should keep rendering.
 *
 * Case-insensitive; matched at the end so a bare name, a workspace-relative
 * path and an absolute path all answer the same way.
 */
const CHAT_SOURCE_PREFERRED_EXT = /\.(?:md|mdx)$/i;

/** Which editor action opens a chat-side file reference. */
export function pickOpenActionForPath(path: string): "openFile" | "openPreview" {
    return isWorkspacePreviewable(path) && !CHAT_SOURCE_PREFERRED_EXT.test(path)
        ? "openPreview"
        : "openFile";
}