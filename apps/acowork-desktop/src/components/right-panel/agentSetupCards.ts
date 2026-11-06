/**
 * agentSetupCards — the Setup panel's card order and its "which cards start
 * expanded" rule (pure, unit-tested).
 *
 * Why a registry instead of seven scattered `useState(true)`:
 *
 *   The panel grew from four cards to seven when the (since removed) Tools panel's Builtin /
 *   MCP cards and the Debug panel's Prompts & Skills card moved in (the Tools
 *   tab no longer exists as a rail entry). Seven cards all expanded is a
 *   scroll wall, so exactly the FIRST `DEFAULT_EXPANDED_CARD_COUNT` cards open
 *   on mount and every card after them starts collapsed.
 *
 *   That rule has to be stated once. If each card kept its own literal initial
 *   value, inserting a card in the middle would silently break the count and
 *   the panel would drift into "some cards open, some closed" for no reason.
 *   So render order IS the single source of truth: `AGENT_SETUP_CARDS` lists
 *   the cards top-to-bottom, and `defaultOpenForCard` derives the initial
 *   value from a card's position in that list.
 *
 *   The behaviour is initial-only — the user can still open all seven, and
 *   nothing re-collapses them. Card open state is component-local (no
 *   persistence), which is why there is no stored value to migrate here.
 */

/** Every level-1 card the Setup panel renders, in render order. */
export type AgentSetupCardId =
  | "agentInfo"
  | "sessionParams"
  | "modelParams"
  | "chatRouter"
  | "promptsSkills"
  | "builtinTools"
  | "mcpServers";

/** Top-to-bottom card order. Position decides the initial expand state. */
export const AGENT_SETUP_CARDS: readonly AgentSetupCardId[] = [
  "agentInfo",
  "sessionParams",
  "modelParams",
  "chatRouter",
  "promptsSkills",
  "builtinTools",
  "mcpServers",
];

/** How many leading cards start expanded; the rest start collapsed. */
export const DEFAULT_EXPANDED_CARD_COUNT = 3;

/** Whether a card starts expanded, derived from its position in the registry. */
export function defaultOpenForCard(id: AgentSetupCardId): boolean {
  const index = AGENT_SETUP_CARDS.indexOf(id);
  return index >= 0 && index < DEFAULT_EXPANDED_CARD_COUNT;
}

/**
 * The initial open-state map for the whole panel. One object instead of seven
 * `useState` calls: every `ExpandableRow` reads its own key, and `toggleCard`
 * flips one key, so the cards can never disagree with the registry about
 * which ones are "the first three".
 */
export function initialCardOpenState(): Record<AgentSetupCardId, boolean> {
  const state = {} as Record<AgentSetupCardId, boolean>;
  for (const id of AGENT_SETUP_CARDS) state[id] = defaultOpenForCard(id);
  return state;
}
