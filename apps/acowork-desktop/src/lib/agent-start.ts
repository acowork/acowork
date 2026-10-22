//! Agent start orchestration utility.
//!
//! Provides `startAgentAndSyncUI` — an atomic follow-up that:
//! 1. Waits for the Runtime to become ready
//! 2. Resolves + opens the active session (single resolver:
//!    `agentStore.resolveActiveSession`), then populates the session list
//! 3. Synchronizes UI (fetch workspaces, refresh config)
//!
//! All callers (AgentList right-click, ChatPanel "Start Agent" button)
//! chain this through `useAgentStore.tryStartAgent`'s `run` option — the
//! gate there owns the dedup AND the actual `startAgent` call, so session
//! data is always ready before rendering.

import { useAgentStore, type ResolveActiveSessionResult } from "../stores/agentStore";
import { useChatStore } from "../stores/chatStore";
import { useWorkspaceStore } from "../stores/workspaceStore";
import { emitAgentConfigRefresh } from "./refresh";

/**
 * Resolve + open the agent's active session, then populate the session
 * list so the tab bar / sidebar show the right title.
 *
 * Resolution (latest-session tri-state → own-list fallback → create)
 * and opening (UI + backend activation + message/config cache) are owned
 * by `agentStore.resolveActiveSession`; this orchestrator only adds the
 * start-window retry budget, because a freshly started Runtime may not
 * have answered SESSIONS_READY yet.
 */
async function initSessionForAgent(agentId: string): Promise<void> {
  // ponytail: diagnostic
  const __i0 = performance.now();
  // Retry until the Runtime reaches SESSIONS_READY with a readable
  // session (max 10 attempts, 1s interval). `unavailable` is the only
  // outcome worth retrying — `noop` / `opened` / `created` are terminal.
  const maxRetries = 10;
  let result: ResolveActiveSessionResult = "unavailable";
  for (let i = 0; i < maxRetries; i++) {
    const __t0 = performance.now();
    result = await useAgentStore.getState().resolveActiveSession(agentId);
    // ponytail: diagnostic
    console.warn(
      `[agent-start] initSessionForAgent retry=${i} ` +
      `after ${Math.round(performance.now() - __t0)}ms ` +
      `result=${result} ` +
      `elapsed=${Math.round(performance.now() - __i0)}ms`,
    );
    if (result !== "unavailable") break;
    if (i < maxRetries - 1) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
  if (result === "unavailable") {
    // Never reached SESSIONS_READY within the retry budget. Do NOT
    // create a session here — that is exactly the duplicate race
    // ADR-085 D7 removes. Surface the failure; the start flow's
    // caller shows it and the user can retry.
    throw new Error(
      `Agent ${agentId} session state unavailable (still starting or unreachable)`,
    );
  }
  if (result === "created") {
    // The fresh session is activated by the `session_created` event
    // handler (`activateNewlyCreatedSession`) — nothing to open here.
    return;
  }

  // Populate `agents[agentId].sessions` (sidebar + session-tab titles).
  // On cold start the `/sessions` endpoint races the disk scan and may
  // 503 / return an empty list while `/latest-session` (which reads the
  // in-memory cache) already resolved. Without this retry the
  // SessionTabBar would mount before `sessions[]` contained the active
  // session and show "Untitled" until the user manually opened the
  // session dropdown — same symptom class as the title-refresh regression
  // pinned in `agentStore.sessionTitle.test.ts` (now covered by the
  // per-session config-mapper path).
  const targetSessionId = useChatStore.getState().getActiveSessionId(agentId);
  if (!targetSessionId) return;
  for (let i = 0; i < maxRetries; i++) {
    await useAgentStore.getState().fetchSessions(agentId);
    const populated = useAgentStore
      .getState()
      .agents[agentId]?.sessions.some(
        (s) => s.session_id === targetSessionId,
      );
    if (populated) break;
    if (i < maxRetries - 1) {
      await new Promise((resolve) => setTimeout(resolve, 1000));
    }
  }
}

/**
 * Atomic agent-start follow-up: wait for readiness, resolve + open the
 * active session, then sync UI (workspaces, config refresh).
 *
 * `tryStartAgent` owns the dedup gate AND the actual `startAgent` call —
 * chain this function through its `run` option so the sidebar's
 * "starting…" badge stays on through session init. Running it directly
 * assumes the agent has already been started.
 *
 * @param agentId  The agent package ID to start.
 */
export async function startAgentAndSyncUI(agentId: string): Promise<void> {
  // ponytail: diagnostic
  const __s0 = performance.now();
  try {
    // 1. Wait for the Runtime to become ready
    await useAgentStore.getState().waitForAgentReady(agentId);
    console.warn(`[agent-start] waitForAgentReady done @${Math.round(performance.now() - __s0)}ms`);

    // 2. Initialize session — resolve + open the active session,
    //    then populate the session list
    await initSessionForAgent(agentId);
    console.warn(`[agent-start] initSessionForAgent done @${Math.round(performance.now() - __s0)}ms`);

    // 3. Sync UI — workspaces, config refresh (pure render)
    useWorkspaceStore.getState().fetchWorkspaces(agentId);
    emitAgentConfigRefresh(agentId);
  } catch (e) {
    // ponytail: diagnostic
    console.warn(
      `[agent-start] startAgentAndSyncUI FAILED @${Math.round(performance.now() - __s0)}ms:`,
      e,
    );
    throw e;
  }
}
