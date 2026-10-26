//! "Add to Chat" — one action, two composers.
//!
//! The right-hand workspace panel is shared by both surfaces of an Agent page
//! (design 27 Wave 5), and each surface owns its own pending-attachment list:
//! the execution view keeps references on the execution session
//! (`chatStore.attachedContext`), the Chat surface keeps them on the Chat
//! session (`chatSessionStore.attachedContext`). Whichever composer is on
//! screen takes the reference — writing to the hidden one is indistinguishable
//! from doing nothing, which is exactly how this read as a bug.
//!
//! A reference is a node-local path the Agent reads live with its own tools;
//! it is NOT a copy of the file (that is what the composer's Paperclip is for).
//! The routed Chat turn carries them in `attached_items` (ADR-046 union), which
//! the Gateway forwards to whichever execution session takes the turn.

import { useMemo } from "react";
import { useAgentStore } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useChatSessionStore } from "../stores/chatSessionStore";
import { isReadOnlySession } from "./session-write-access";
import type { WorkspaceRef } from "./types";

export type { WorkspaceRef };

/** Which composer a reference lands in. */
export type AddToChatTarget =
  | { surface: "chat"; chatSessionId: string }
  | { surface: "exec"; agentId: string; sessionId: string };

/** Pure resolution of the target, so the rule is testable without React.
 *
 *  The surface the tab strip shows wins; when it cannot take the reference
 *  (a Chat surface with no session, a read-only execution session — an
 *  attachment that cannot be sent is not an attachment) the other one does.
 */
export function pickAddToChatTarget(input: {
  agentId: string | null;
  mode: "chat" | "exec" | undefined;
  chatSessionId: string | null;
  execSessionId: string | null;
  execReadOnly: boolean;
}): AddToChatTarget | null {
  const { agentId, mode, chatSessionId, execSessionId, execReadOnly } = input;
  if (!agentId) return null;
  const chat: AddToChatTarget | null = chatSessionId
    ? { surface: "chat", chatSessionId }
    : null;
  const exec: AddToChatTarget | null =
    execSessionId && !execReadOnly
      ? { surface: "exec", agentId, sessionId: execSessionId }
      : null;
  return mode === "chat" ? chat ?? exec : exec ?? chat;
}

/** The composer "Add to Chat" should be writing to right now, or null when no
 *  surface on screen can take a reference at all (the menu entry disables). */
export function useAddToChatTarget(agentId: string | null): AddToChatTarget | null {
  const mode = useChatSessionStore((s) => (agentId ? s.byAgent[agentId]?.mode : undefined));
  const chatSessionId = useChatSessionStore((s) =>
    agentId ? s.byAgent[agentId]?.chatSessionId ?? null : null,
  );
  const execSessionId = useChatStore((s) =>
    agentId ? s.agentStates[agentId]?.activeSessionId ?? null : null,
  );
  const execReadOnly = useAgentStore((s) => {
    if (!agentId || !execSessionId) return false;
    return isReadOnlySession(
      s.agents[agentId]?.sessions.find((x) => x.session_id === execSessionId)?.can_write,
    );
  });
  return useMemo(
    () => pickAddToChatTarget({ agentId, mode, chatSessionId, execSessionId, execReadOnly }),
    [agentId, mode, chatSessionId, execSessionId, execReadOnly],
  );
}

/** Land a reference in the target composer. No-op on a null target: callers
 *  gate their affordance on it, so this is the race guard, not the UX. */
export function attachWorkspaceRef(
  target: AddToChatTarget | null,
  ref: WorkspaceRef,
): void {
  if (!target) return;
  if (target.surface === "chat") {
    useChatSessionStore.getState().addAttachedContext(target.chatSessionId, ref);
  } else {
    useChatStore.getState().addAttachedContext(target.agentId, target.sessionId, ref);
  }
}
