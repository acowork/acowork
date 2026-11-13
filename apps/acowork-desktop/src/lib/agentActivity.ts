// The word that says what an agent is doing right now.
//
// Before this existed, a send in a Chat earned one sentence for the whole
// turn — "SSE.Cassie is working on it…" — while the runtime had been naming
// the actual phase the entire time (`session_state`, pushed within ~50 ms of
// every transition, ADR-049) and the tool line already carried the tool's own
// progress label (`tools.<name>.running`). Three vocabularies for one fact,
// and the one the user watches was the least informative. This is the single
// map all three surfaces read: the 1:1 wait line (`AgentChatView`), the group
// wait line (`GroupChatPanel`), the run-block head (`ChatExecutionBlock`) and
// the execution view's phase line (`ChatPanel`).
//
// The input is the Runtime's `SessionStatus` — a fact, not an inference:
// `tool_executing` carries the names of the tools actually dispatched
// (`core/acowork-runtime/src/agent/loop_tools.rs`, paired with the
// `tools.<name>.running` table so a name resolves to a word in all five
// locales instead of a hand-written guess per surface).
//
// What it deliberately does NOT do: it never picks a tool out of a transcript,
// and a caller that cannot tell which run its turn went to passes `null` —
// `liveActivityStatus` is the guard for that, because a word drawn from a
// member's older or unrelated session is a lie dressed up as a status.

import type { SessionStatus } from "./types";
import enMessages from "../i18n/locales/en.json";

/** The app's own `t` shape (see `lib/saveProbeFeedback.ts`) — this is *a*
 *  translator, not i18next's branded `TFunction`, and the components hand
 *  exactly this to it. */
type Translate = (key: string, options?: Record<string, unknown>) => string;

export interface ActivityWord {
  text: string;
  /**
   * False when the word is only the generic fallback — the caller then keeps
   * its own "working on it" sentence, which at least carries the agent's name.
   */
  specific: boolean;
}

/**
 * MCP tool names are `mcp_<server>__<tool>`; the server is what tells the user
 * what kind of work is happening (memory, the project board, the doc library),
 * and the built-in tool table has no entry for any of them. Only the three
 * servers the platform ships are listed — any other name falls through to the
 * tool-named word, which is honest where a guessed category is not.
 */
const MCP_SERVER_ACTIVITY: Record<string, string> = {
  memory: "chatView.activity.memory",
  pm: "chatView.activity.project",
  doc: "chatView.activity.docs",
};

/**
 * Does the vocabulary have a word for this tool? `en` is `fallbackLng`, so a
 * label missing there is missing in every locale — and a key built from a
 * variable is invisible to `check-i18n.mjs` (its own header says so), which
 * means the only thing standing between an unfamiliar tool name and a key path
 * painted across the UI is this check.
 */
function hasToolLabel(name: string): boolean {
  const table = (enMessages as { tools?: Record<string, { running?: string }> }).tools;
  return typeof table?.[name]?.running === "string";
}

/** A tool name → the phrase describing the work, or null when the table has
 *  nothing for it. */
function toolActivityKey(name: string): string | null {
  const mcp = /^mcp_([^_]+)__.+/.exec(name);
  if (mcp) return MCP_SERVER_ACTIVITY[mcp[1]] ?? null;
  // Built-in tools register a `running` label — that table is the vocabulary
  // the tool line already uses, so the wait line cannot drift from what the
  // expanded block says. A custom/MCP tool with no entry gets named instead.
  return hasToolLabel(name) ? `tools.${name}.running` : null;
}

/**
 * The word for one status. `idle` / `paused` / a status we have not seen yet
 * are NOT activity: they answer "what is it doing" with "nothing, and you
 * should know about it", which is the banners' job, not this line's.
 */
export function agentActivity(
  status: SessionStatus | null | undefined,
  t: Translate,
): ActivityWord {
  const generic = (key: string, opts?: Record<string, string>): ActivityWord => ({
    text: t(key, opts),
    specific: false,
  });

  if (!status) return generic("chatView.activity.working");

  switch (status.status) {
    case "tool_executing": {
      const names = status.detail?.tools ?? [];
      if (names.length === 0) return generic("chatView.activity.working");
      const key = toolActivityKey(names[0]);
      if (!key) {
        // An unfamiliar tool still deserves a changing word: naming it is
        // honest where a generic sentence is not.
        return { text: t("chatView.activity.tool", { name: names[0] }), specific: true };
      }
      return { text: t(key), specific: true };
    }
    // `llm_awaiting_first_chunk` is "request sent, no first chunk yet" and
    // `thinking` is reasoning tokens streaming. Both read as the model working
    // the problem; splitting them gives the user a word that changes for a
    // reason they cannot act on. The dot colour on the execution surface still
    // tells them apart.
    case "thinking":
    case "llm_awaiting_first_chunk":
      return { text: t("chatView.activity.analyzing"), specific: true };
    case "llm_streaming":
      return { text: t("chatView.activity.composing"), specific: true };
    case "waiting_approval":
      return { text: t("chatView.activity.waitingApproval"), specific: true };
    default:
      return generic("chatView.activity.working");
  }
}

/**
 * Pick the status to speak for a turn, from the runs that could honestly be
 * that turn.
 *
 * One busy run → its word. None busy → nothing (the caller's own fallback, and
 * the right answer while the router is still deciding which run this is). More
 * than one → nothing: naming one member's tool for a turn addressed to several
 * is the same guess design 27 §5.3.1 already refuses to make about the avatar.
 */
export function liveActivityStatus(
  statuses: (SessionStatus | null | undefined)[],
): SessionStatus | null {
  const busy = statuses.filter(
    (s): s is SessionStatus => !!s && s.status !== "idle" && s.status !== "paused",
  );
  if (busy.length !== 1) return null;
  return busy[0];
}
