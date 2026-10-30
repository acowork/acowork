//! Reasoning effort level model — the frontend mirror of the Rust
//! `ReasoningEffort` enum (`core/acowork-core/src/providers/traits.rs`).
//!
//! Two responsibilities:
//!   1. The canonical order of the depth levels (used to sort a model's
//!      declared support set and to clamp a chosen level into it).
//!   2. The list of levels to offer in the UI for a given model: the model's
//!      own declared support set (models.dev `reasoning_options[].values`)
//!      when known, the generic set otherwise (custom model, no catalog
//!      entry).
//!
//! `Auto` and `Off` are always offered — they carry no depth level and are
//! therefore valid for every model. `Off` means "do not control reasoning"
//! (the reasoning parameter is omitted); it never means "send `none`".

/** The depth levels, in ascending order of reasoning depth. */
export const REASONING_DEPTH_LEVELS = [
  "minimal",
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
] as const;

export type ReasoningDepthLevel = (typeof REASONING_DEPTH_LEVELS)[number];

/** The generic set offered when a model declares no support set. */
export const GENERIC_REASONING_LEVELS: readonly string[] = [
  "low",
  "medium",
  "high",
  "xhigh",
  "max",
];

/** All level spellings the backend accepts (`from_str_loose`). */
const KNOWN_LEVELS = new Set<string>([
  "auto",
  "off",
  "none",
  "default",
  ...REASONING_DEPTH_LEVELS,
]);

/** Rank of a level; `undefined` for `auto`/`off` and unknown spellings. */
export function reasoningLevelRank(level: string): number | undefined {
  const idx = REASONING_DEPTH_LEVELS.indexOf(level as ReasoningDepthLevel);
  return idx === -1 ? undefined : idx;
}

/** Normalise a catalog spelling to the canonical one
 *  (`none` → `off`, `default` → `auto`). Returns `undefined` for unknown. */
export function normalizeReasoningLevel(level: string): string | undefined {
  const l = level.trim().toLowerCase();
  if (!KNOWN_LEVELS.has(l)) return undefined;
  if (l === "none") return "off";
  if (l === "default") return "auto";
  return l;
}

/**
 * The levels to offer for a model.
 *
 * `supported` is the model's declared support set (models.dev
 * `reasoning_options[].values`). When it is missing or contains no known
 * level, the generic set is returned — a custom model must still be
 * controllable.
 *
 * The result never contains `auto`/`off`: those two are added by the caller
 * (they are always valid).
 */
export function reasoningLevelsFor(supported?: readonly string[]): string[] {
  if (!supported || supported.length === 0) return [...GENERIC_REASONING_LEVELS];
  const known = supported
    .map(normalizeReasoningLevel)
    .filter((l): l is string => !!l && reasoningLevelRank(l) !== undefined);
  if (known.length === 0) return [...GENERIC_REASONING_LEVELS];
  // Deduplicate, then sort by depth.
  const unique = [...new Set(known)];
  unique.sort((a, b) => (reasoningLevelRank(a) ?? 0) - (reasoningLevelRank(b) ?? 0));
  return unique;
}

/**
 * Clamp a chosen level into the model's support set.
 *
 * Mirrors `ReasoningEffort::clamp_to_supported` on the Rust side: the
 * deepest supported level that is not deeper than the request wins; when the
 * model supports nothing at or below the request, its shallowest supported
 * level is used. `auto`/`off` pass through unchanged.
 */
export function clampReasoningLevel(level: string, supported?: readonly string[]): string {
  const normalized = normalizeReasoningLevel(level);
  if (!normalized) return level;
  const requested = reasoningLevelRank(normalized);
  if (requested === undefined) return normalized;

  const levels = reasoningLevelsFor(supported);
  const ranks = levels
    .map(reasoningLevelRank)
    .filter((r): r is number => r !== undefined);
  if (ranks.length === 0) return normalized;

  const atOrBelow = [...levels]
    .reverse()
    .find((l) => (reasoningLevelRank(l) ?? 0) <= requested);
  if (atOrBelow) return atOrBelow;
  return levels[0];
}
