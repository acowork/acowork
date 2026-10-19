import { useRef, useState, useLayoutEffect } from "react";
import type { ModelEntry, PendingAttachedItem } from "../../lib/types";
import { log } from "../../lib/logger";

// ── Types ────────────────────────────────────────────────────────────

// Re-export the unified `PendingAttachedItem` shape for backwards compat
// at the consumer level. The legacy `PendingFile` / `PendingImage` types
// were removed in ADR-046: a single queue now holds documents, images,
// and workspace refs (all of which become an `AttachedItem` once resolved).
export type { PendingAttachedItem };

/**
 * Per-session mutable ref state. Stores values that need to persist across
 * renders within a session but don't trigger re-renders when written.
 *
 * IMPORTANT: This ref only holds PER-SESSION state.  Per-user-intent UI
 * state (such as scroll-pinned-to-bottom, which survives across session
 * switches and is owned by ChatPanel via its own ref) MUST NOT live here.
 *
 * All fields are reset atomically when the active session changes within
 * the same component instance, so per-session state never leaks across
 * sessions.  A fresh component instance starts with default values
 * (created by useRef(default)), so no reset is needed on first mount.
 */
export interface SessionScope {
  // ── Scroll & Virtual ──
  /** True while the LLM is in its "thinking" prelude (before any text streamed). */
  thinkingWasShowing: boolean;
  /** Previous display count for scroll-on-new-message logic. */
  prevDisplayCount: number;

}

/** Factory: returns a fresh default SessionScope. */
export function createDefaultSessionScope(): SessionScope {
  return {
    thinkingWasShowing: false,
    prevDisplayCount: 0,
  };
}

// ── Hook ─────────────────────────────────────────────────────────────

export interface SessionScopeAPI {
  /** Mutable scope ref — read/write without triggering re-renders. */
  scope: React.MutableRefObject<SessionScope>;

  // ── State (triggers re-render on change) ──
  inputValue: string;
  setInputValue: (v: string) => void;

  pendingAttachedItems: PendingAttachedItem[];
  setPendingAttachedItems: React.Dispatch<React.SetStateAction<PendingAttachedItem[]>>;

  showImageUnsupportedDialog: boolean;
  setShowImageUnsupportedDialog: (v: boolean) => void;

  imageCapableModels: ModelEntry[];
  setImageCapableModels: React.Dispatch<React.SetStateAction<ModelEntry[]>>;

  todosCollapsed: boolean;
  setTodosCollapsed: (v: boolean) => void;
}

/**
 * Consolidates all per-session refs and state into a single hook.
 *
 * On GENUINE SESSION CHANGE within the same component instance (current
 * session ID differs from the previous render), the entire SessionScope
 * ref is replaced with a fresh default and all state values are reset.
 * This eliminates the class of bugs where per-session refs/state leak
 * across session switches because individual variables were not reset.
 *
 * On FRESH MOUNT (first render of a new component instance), no reset
 * is needed: scopeRef is already initialized to its default state by
 * useRef(createDefaultSessionScope()), and useState values start at their
 * initial defaults.
 */
export function useSessionScope(
  currentSessionId: string | null,
  onSessionChange?: () => void,
): SessionScopeAPI {
  const scopeRef = useRef<SessionScope>(createDefaultSessionScope());
  const prevSessionRef = useRef<string | null>(null);

  // ── State values (trigger re-render) ──
  const [inputValue, setInputValue] = useState("");
  const [pendingAttachedItems, setPendingAttachedItems] = useState<PendingAttachedItem[]>([]);
  const [showImageUnsupportedDialog, setShowImageUnsupportedDialog] = useState(false);
  const [imageCapableModels, setImageCapableModels] = useState<ModelEntry[]>([]);
  // Todos start collapsed: the header row already carries progress + current
  // task, so the list only expands when the user clicks the toggle.
  const [todosCollapsed, setTodosCollapsed] = useState(true);

  // ── Reset on session change ──
  useLayoutEffect(() => {
    log.debug("[useSessionScope] reset-effect fire", {
      currentSessionId,
      prevSessionRef: prevSessionRef.current,
    });
    if (currentSessionId === prevSessionRef.current) return;

    // Capture the previous value BEFORE overwriting so we can distinguish a
    // fresh mount (prev was null) from a genuine session change (prev was a
    // non-null session ID different from the new one).
    const isFreshMount = prevSessionRef.current === null;
    prevSessionRef.current = currentSessionId;

    if (isFreshMount) {
      log.debug("[useSessionScope] fresh mount — no reset performed", {
        currentSessionId,
      });
      // Fresh mount: scope is already in default state (useRef(default)) and
      // useState values already hold their initial defaults.  No reset needed.
      // (Per-user-intent scroll state such as pinnedToBottom is owned OUTSIDE
      // this hook — by ChatPanel via its own ref — so it is not affected.)
      return;
    }

    // Genuine session change within the same component instance — reset all
    // per-session state to clear leaks across sessions.
    scopeRef.current = createDefaultSessionScope();
    setInputValue("");
    setPendingAttachedItems([]);
    setShowImageUnsupportedDialog(false);
    setImageCapableModels([]);
    setTodosCollapsed(true);
    onSessionChange?.();
  }, [currentSessionId, onSessionChange]);

  return {
    scope: scopeRef,
    inputValue,
    setInputValue,
    pendingAttachedItems,
    setPendingAttachedItems,
    showImageUnsupportedDialog,
    setShowImageUnsupportedDialog,
    imageCapableModels,
    setImageCapableModels,
    todosCollapsed,
    setTodosCollapsed,
  };
}