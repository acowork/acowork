//! Model capability resolution helpers.
//!
//! `setCurrentModel` (user clicks the model dropdown) and the MQTT
//! `model_confirmed` handler (Runtime confirms a model switch) both need
//! to derive the new model's `reasoning_effort` from the cached
//! `availableModels` list. This module is the single place that defines
//! the lookup so the two paths can't drift.
//!
//! The resolution chain mirrors the backend's
//! `resolve_effective_reasoning_effort` (llm_effects.rs):
//!
//!   1. `default_reasoning_effort` - provider's recommended default
//!   2. `supports_reasoning == true` -> `"auto"` - model is
//!      reasoning-capable but has no explicit default
//!   3. `null` - model doesn't support reasoning
//!
//! This ensures the frontend's optimistic update (before the backend
//! confirms) matches the backend's authoritative value, minimizing
//! UI flicker. The backend's `session_config` MQTT message is always
//! the final source of truth and will overwrite any optimistic value.

import type { ModelEntry } from "./types";

/**
 * Resolve the `default_reasoning_effort` for `(model, provider)` from
 * the supplied `availableModels` list.
 *
 * Returns `null` when:
 *   - the model isn't in the list
 *   - the model doesn't support reasoning (`reasoning` is falsy)
 *
 * The lookup is exact-match on both `name` AND `provider` because the
 * same model name can exist under multiple providers (e.g.
 * `claude-3-5-sonnet` on both Anthropic and Bedrock).
 */
export function resolveDefaultReasoningEffort(
  models: readonly ModelEntry[],
  model: string,
  provider: string,
): string | null {
  const entry = models.find((m) => m.name === model && m.provider === provider);
  return defaultEffortOf(entry);
}

/**
 * Same resolution chain as [`resolveDefaultReasoningEffort`] but for an
 * already-located model entry (the Chat composer resolves its own
 * `(name, provider)` match and must not re-implement the chain — that
 * drift caused reasoning-capable models without an explicit
 * `default_reasoning_effort` (e.g. custom-provider models) to hide the
 * effort toggle on the Chat surface while the execution surface showed it).
 */
export function defaultEffortOf(
  entry: Pick<ModelEntry, "reasoning" | "default_reasoning_effort"> | null | undefined,
): string | null {
  if (!entry) return null;

  // Level 1: provider-recommended default.
  if (entry.default_reasoning_effort) {
    return entry.default_reasoning_effort;
  }

  // Level 2: supports_reasoning -> Auto (aligns with backend
  // resolve_effective_reasoning_effort Level 3).
  if (entry.reasoning === true) {
    return "auto";
  }

  // Level 3: model doesn't support reasoning.
  return null;
}
