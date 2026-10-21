// Design 27 Wave 5 (revised — WeChat model): the Agent page. Replaces the
// bare <ChatPanel /> in AppLayout's middle slot.
//
// One tab strip, two surfaces that CO-EXIST (mutual exclusion was the bug
// the first cut shipped with):
//
//   pinned tab — the pair's single Chat session (plan 5.1/5.2: opening an
//                Agent always lands on it, it never closes). Label is the
//                agent's display name, like a WeChat contact title — the
//                session's stored `title` is never shown.
//   other tabs — execution sessions, opened/closed freely through the
//                existing +/🕐/status-card paths. Closing the last one
//                lands back on the chat tab; opening one only flips which
//                surface the strip shows, never removes the chat tab.
//
// The fallback IS the rollback (runbook intent-chat-session-rollback):
// when there is no account (local mode) or the Chat surface cannot be
// reached, `openAgent` resolves with `chatSessionId: null` and the page
// renders the pre-Wave-5 ChatPanel — its own tab bar, its own shell —
// without a feature flag.

import { useEffect, useState } from "react";
import { cn } from "../../lib/utils";
import { CAPSULE_PANE_CN } from "../common/capsule";
import { ChatPanel } from "./ChatPanel";
import { SessionTabBar } from "./SessionTabBar";
import { AgentChatView } from "./AgentChatView";
import { ChatHistoryDialog } from "./ChatHistoryDialog";
import { useAgentStore } from "../../stores/agentStore";
import { useChatSessionStore } from "../../stores/chatSessionStore";

/** Same shell the ChatPanel root uses — the two surfaces are one pane. */
const SHELL_CN = cn(CAPSULE_PANE_CN, "min-w-[338px] flex-1 bg-chat-body");

export function AgentPage() {
  const selectedAgentId = useAgentStore((s) => s.selectedAgentId);
  const agentMeta = useAgentStore((s) =>
    selectedAgentId ? s.agents[selectedAgentId]?.meta : undefined,
  );
  const st = useChatSessionStore((s) =>
    selectedAgentId ? s.byAgent[selectedAgentId] : undefined,
  );
  const alive = !!agentMeta?.alive;
  // In-conversation history dialog (ADR-097): state lives here so the trigger
  // can render in `SessionTabBar`'s title cluster while `AgentChatView` keeps
  // the locate/scroll effect. Declared before the early returns (rules of hooks).
  const [historyOpen, setHistoryOpen] = useState(false);

  useEffect(() => {
    // Resolve the Chat session only once the agent runs. While it is
    // stopped the page renders the legacy start screen anyway, and the
    // pair cannot have a session yet (creating one needs the Runtime),
    // so no fetch is issued. `openAgent` short-circuits when already
    // resolved, so alive flips never refetch.
    if (selectedAgentId && alive) void useChatSessionStore.getState().openAgent(selectedAgentId);
  }, [selectedAgentId, alive]);

  if (!selectedAgentId) return null;

  // Agent stopped: keep the pre-Wave-5 interaction — the full ChatPanel
  // renders its "start agent" screen (big Play button over the empty
  // chat shell). A Chat session shown here would be unsendable: turns
  // need a live Runtime to answer. The tab strip returns as soon as the
  // agent comes up.
  if (!alive) {
    return (
      <div className="flex min-w-0 flex-1">
        <ChatPanel />
      </div>
    );
  }

  // Resolving gate: a centered dot inside the same shell, so the pane
  // never flashes empty and the column width never jumps.
  if (!st || !st.resolved) {
    return (
      <div className={cn(SHELL_CN, "flex items-center justify-center")}>
        <span className="h-2 w-2 animate-pulse rounded-full bg-[var(--color-accent)]" />
      </div>
    );
  }

  // No Chat surface reachable (local mode / user proxy down): the entire
  // pre-Wave-5 page, tab bar included — the rollback path.
  if (!st.chatSessionId) {
    return (
      <div className="flex min-w-0 flex-1">
        <ChatPanel />
      </div>
    );
  }

  const chatSessionId = st.chatSessionId;
  const agentName = agentMeta?.display_name ?? agentMeta?.name ?? "";

  return (
    <div className={cn(SHELL_CN, "flex min-h-0 flex-col")}>
      <SessionTabBar
        agentId={selectedAgentId}
        chatTab={{
          label: agentName,
          active: st.mode === "chat",
          onSelect: () => useChatSessionStore.getState().setMode(selectedAgentId, "chat"),
        }}
        onOpenChatHistory={() => setHistoryOpen(true)}
      />
      {st.mode === "chat" ? (
        <AgentChatView agentId={selectedAgentId} sessionId={chatSessionId} />
      ) : (
        <ChatPanel embedded />
      )}
      {/* In-conversation history dialog (ADR-097). Owned at the page level so
          its trigger can live in `SessionTabBar`'s title cluster while the
          locate effect stays in `AgentChatView` (which subscribes to the
          window's `pendingLocateSeq`). Only meaningful on the Chat surface. */}
      <ChatHistoryDialog
        open={historyOpen}
        onClose={() => setHistoryOpen(false)}
        chatSessionId={chatSessionId}
      />
    </div>
  );
}
