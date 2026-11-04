/**
 * Ownership registry for the window-level OS file drop (`desktop://file-drop`).
 *
 * Tauri captures an OS file drop at the Rust layer and forwards the absolute
 * paths to EVERY webview listener (`src-tauri/src/lib.rs`,
 * `WindowEvent::DragDrop`). That is right while the chat is the only drop
 * surface, but a modal that also accepts dropped files — the Agent library
 * dialog's Local tab — would otherwise fight it: one `.agent` dropped on the
 * dialog would install AND land in the chat as an attachment.
 *
 * A modal therefore *claims* the drop for as long as it is mounted; the chat
 * upload path yields while any claim is held. Claims are keyed by owner id so
 * two overlapping claimants cannot release each other's claim by accident.
 */
const claims = new Set<string>();

export function claimFileDrop(owner: string): void {
  claims.add(owner);
}

export function releaseFileDrop(owner: string): void {
  claims.delete(owner);
}

export function isFileDropClaimed(): boolean {
  return claims.size > 0;
}
