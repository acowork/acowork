// SPDX-License-Identifier: Apache-2.0
// Copyright the ACowork project contributors.
//
// Workspace-panel "is this file previewable?" rule.
//
// Centralised so the workspace tree (double-click, search select, right-click
// menu) and the editor panel (preview/source toggle) can't drift out of sync.
// The single source of truth replaces four near-duplicate regexes that were
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
//   - Everything else (JSON, source code, configs) → Monaco source editor.
//
// NB: this is the **workspace-panel** rule. The chat-side rule lives in
// `markdownLinkResolver.pickOpenActionForPath` and is intentionally
// different — images only — because a chat link to a `.md` file is expected
// to keep rendering as Markdown. Don't unify them.

/** Extensions that the workspace panel treats as previewable. */
export const WORKSPACE_PREVIEW_EXT_RE =
    /\.(?:jpg|jpeg|png|gif|webp|svg|md|html?)$/i;

/**
 * Decide whether `name` (a basename or relative path) should open in preview
 * mode from a workspace-panel entry point. Both forward and backward
 * slashes are tolerated because the file tree sometimes hands us a
 * workspace-relative POSIX path even on Windows.
 */
export function isWorkspacePreviewable(name: string): boolean {
    return WORKSPACE_PREVIEW_EXT_RE.test(name);
}