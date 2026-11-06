//! ADR-104 — which recommendation states count as "handled", and the group's
//! default-expand rule derived from it.
//!
//! `state` is derived server-side (`build_recommendation_views` in the
//! Runtime) from `agent_mcp.json` × the merged install list; this module never
//! re-joins a manifest against an install list (ADR-104 rejects a second
//! client-side implementation — two clients would eventually disagree). It only
//! *classifies* the four states the Runtime already chose:
//!
//! | state                | handled | why                                              |
//! |----------------------|---------|--------------------------------------------------|
//! | `active`             | yes     | the server is on for this agent — nothing to do  |
//! | `dismissed`          | yes     | the user refused it; refusal is a decision       |
//! | `installed_inactive` | no      | installed but not started — one click left       |
//! | `not_installed`      | no      | nothing installed yet                            |
//!
//! Dismissal counts as handled for the same reason the agent-list badge
//! ignores it (ADR-104 §5.3): nagging a user who already said "no" is not a
//! reminder, it is noise. The group's header still carries the total count, so
//! collapsing never hides the suggestions — it only stops them from claiming
//! the panel.

import type { McpRecommendationState, McpRecommendationView } from "./types";

/** A suggestion the user has either adopted (`active`) or refused (`dismissed`). */
export function isHandledRecommendation(state: McpRecommendationState): boolean {
  return state === "active" || state === "dismissed";
}

/**
 * True while at least one suggestion still needs a user decision — i.e. while
 * the group must be visible without a click.
 *
 * Takes the state field only (`{ state }`), because callers that have already
 * projected the rows as something narrower can still ask the question.
 */
export function hasUnhandledRecommendation(
  rows: readonly Pick<McpRecommendationView, "state">[],
): boolean {
  return rows.some((row) => !isHandledRecommendation(row.state));
}
