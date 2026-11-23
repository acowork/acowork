// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Workspace-panel "is this file previewable?" rule, and the hosted-app
// extension table every preview decision reads (ADR-118).
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
//     and declared the extension in `ui.canvas.extensions` (ADR-118): the
//     preview is that app's iframe in the editor pane, never a chat bubble.
//     Office documents (docx / xlsx / pptx / pdf) keep their built-in
//     viewers until an office craft app is enabled; camera raw, HEIF/JXL/
//     PSD photos and vector interchange files (ai / eps / dxf / …) have no
//     built-in viewer, so a craft app is their only in-app renderer.
//   - Everything else (JSON, source code, configs) → Monaco source editor.
//
// NB: this is the **workspace-panel** rule. The chat-side rule lives in
// `markdownLinkResolver.pickOpenActionForPath` and is intentionally
// different — images only — because a chat link to a `.md` file is expected
// to keep rendering as Markdown. Don't unify them.

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