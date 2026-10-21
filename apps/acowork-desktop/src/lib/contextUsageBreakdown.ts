export type ContextUsageCategoryKey =
  | "system"
  | "tools"
  | "messages"
  | "connectors"
  | "skills";

/**
 * Per-section metadata surfaced to the frontend by the runtime's
 * `onContextBuilt` event (see `core/acowork-core/proto/mqtt_payload.proto`
 * `ContextSectionMeta`). We deliberately consume `size_bytes` — the real
 * byte length of the assembled section content — and *not*
 * `token_estimate`, because the per-section `token_estimate` is computed
 * independently for each section via `crate::token::count_text` and the
 * per-section char/token heuristic can drift 1.05×–1.15× across
 * sections, which compounds into misleading row percentages. Bytes are
 * exact, identical per section, and are the right denominator for
 * computing the relative weight of each category.
 *
 * The popover header still shows the *billed* `contextUsage.usage_percent`
 * reported by the LLM provider (the authoritative number), and we scale
 * the byte-derived share by that exact value — so the displayed
 * percentages stay internally consistent with the header.
 */
export interface ContextUsageSection {
  key: string;
  size_bytes: number;
}

export interface ContextUsageCategory {
  key: ContextUsageCategoryKey;
  percentage: number;
}

const CATEGORY_ORDER: ContextUsageCategoryKey[] = [
  "system",
  "tools",
  "messages",
  "connectors",
  "skills",
];
const TOOL_SECTION_KEYS = new Set(["tool_definitions", "tools"]);
// `skill_instructions` = the Progressive-mode summary (+ any skill the user
// explicitly picked for this turn) injected into the system prompt.
// `skills_loaded` = full SKILL.md bodies the model pulled via the
// `load_skill` tool; they arrive as Tool messages in history but belong to
// the skills category, not messages — without this key
// `categoryForSection` would silently fall through to "system".
const SKILL_SECTION_KEYS = new Set(["skill_instructions", "skills", "skills_loaded"]);
const MESSAGE_SECTION_KEYS = new Set(["messages", "message_history", "todo_context"]);
const CONNECTOR_SECTION_KEYS = new Set([
  "connectors",
  "connector",
  "connector_context",
  "connector_tool_definitions",
]);

function categoryForSection(key: string): ContextUsageCategoryKey {
  if (MESSAGE_SECTION_KEYS.has(key)) return "messages";
  if (TOOL_SECTION_KEYS.has(key)) return "tools";
  if (SKILL_SECTION_KEYS.has(key)) return "skills";
  if (CONNECTOR_SECTION_KEYS.has(key)) return "connectors";
  return "system";
}

/**
 * Converts the latest debug context snapshot into the five categories shown
 * by the context-usage popover.
 *
 * Algorithm (ADR-067-style — kept on the frontend because the popover
 * already has the section byte sizes in hand from the `onContextBuilt`
 * MQTT event):
 *
 *   1. Group sections by category and sum `size_bytes`.
 *   2. `byte_share[cat] = Σcat_size_bytes / Σall_size_bytes`
 *   3. `row_pct[cat] = byte_share[cat] × usage_percent`
 *
 * Step 3 anchors the displayed percentages to the **billed** total the
 * LLM reported (which may include completion tokens and may be smaller
 * than the assembled payload after the provider trims / caches). Steps
 * 1–2 stay in bytes because bytes are exact and uniform across
 * sections, whereas `token_estimate` is a heuristic that drifts.
 */
export function computeContextUsageBreakdown(
  sections: readonly ContextUsageSection[],
  usagePercent: number,
): ContextUsageCategory[] {
  const targetPercent = Number.isFinite(usagePercent)
    ? Math.min(100, Math.max(0, usagePercent))
    : 0;
  const byteTotals: Record<ContextUsageCategoryKey, number> = {
    system: 0,
    tools: 0,
    messages: 0,
    connectors: 0,
    skills: 0,
  };

  for (const section of sections) {
    if (!Number.isFinite(section.size_bytes) || section.size_bytes <= 0) continue;
    byteTotals[categoryForSection(section.key)] += section.size_bytes;
  }

  const totalBytes = Object.values(byteTotals).reduce((sum, value) => sum + value, 0);
  const percentages = totalBytes > 0
    ? CATEGORY_ORDER.map((key) => (byteTotals[key] / totalBytes) * targetPercent)
    : CATEGORY_ORDER.map(() => 0);
  const roundedPercentages = percentages.map((value) => roundToOneDecimal(value));

  if (totalBytes > 0) {
    // One-decimal rounding can leave a 0.1 point gap at the top-level total.
    // Assign it to the largest category so the bar and rows stay internally
    // consistent with the percentage shown in the popover header.
    const roundedTotal = roundedPercentages.reduce((sum, value) => sum + value, 0);
    const remainder = roundToOneDecimal(targetPercent - roundedTotal);
    if (remainder !== 0) {
      const largestCategory = roundedPercentages.reduce(
        (largestIndex, value, index, values) => value > values[largestIndex] ? index : largestIndex,
        0,
      );
      roundedPercentages[largestCategory] = roundToOneDecimal(
        Math.max(0, roundedPercentages[largestCategory] + remainder),
      );
    }
  }

  return CATEGORY_ORDER.map((key, index) => ({
    key,
    percentage: roundedPercentages[index],
  }));
}

function roundToOneDecimal(value: number): number {
  return Math.round((value + Number.EPSILON) * 10) / 10;
}

/**
 * Redistribute a real, billed token total across the assembled sections
 * in proportion to their `size_bytes` byte share.
 *
 * The debug panel snapshots carry `token_estimate` per section (computed
 * independently via `crate::token::count_text`). When those per-section
 * heuristics are summed naively the result drifts from the LLM's reported
 * `total_tokens` because each section's char→token ratio is slightly
 * different.  Anchoring on bytes (which are exact and uniform) and the
 * authoritative billed total makes the per-section numbers sum to the
 * exact billed total — and keeps them internally consistent with the
 * `contextUsage` numbers rendered next to the debug panel.
 *
 * Algorithm:
 *
 *   section_tokens ≈ totalTokens × section.size_bytes / Σsection.size_bytes
 *
 * Returns a `Record<sectionKey, tokens>` of non-negative integers that
 * sum to exactly `Math.round(totalTokens)`.  Returns an empty record
 * when no byte data is available — callers should fall back to the raw
 * `token_estimate` field in that case.
 *
 * The function is pure and does not mutate the input.
 */
export function redistributeTokensByBytes(
  totalTokens: number,
  sections: readonly { key: string; size_bytes: number }[],
): Record<string, number> {
  const totalBytes = sections.reduce(
    (sum, section) => sum + Math.max(0, Number.isFinite(section.size_bytes) ? section.size_bytes : 0),
    0,
  );
  const billedTotal = Math.max(0, Math.round(Number.isFinite(totalTokens) ? totalTokens : 0));

  if (totalBytes <= 0 || billedTotal <= 0 || sections.length === 0) {
    return {};
  }

  const raw = sections.map((section) => {
    const bytes = Math.max(0, Number.isFinite(section.size_bytes) ? section.size_bytes : 0);
    return (bytes / totalBytes) * billedTotal;
  });
  const floors = raw.map((value) => Math.floor(value));
  let remainder = billedTotal - floors.reduce((sum, value) => sum + value, 0);

  // Distribute leftover tokens (one at a time) to the sections with the
  // largest fractional part.  This guarantees integer outputs whose sum
  // equals `billedTotal` exactly, with each section as close to its true
  // proportional share as integer arithmetic allows.
  const order = raw
    .map((value, index) => ({ index, fractional: value - floors[index] }))
    .sort((a, b) => b.fractional - a.fractional);

  for (const { index } of order) {
    if (remainder <= 0) break;
    floors[index] += 1;
    remainder -= 1;
  }

  const out: Record<string, number> = {};
  sections.forEach((section, index) => {
    out[section.key] = floors[index];
  });
  return out;
}

/** Keep one decimal in the detailed popover while other status surfaces retain their integer format. */
export function formatDetailedPercent(value: number): string {
  if (!Number.isFinite(value)) return "0.0";
  return Math.min(100, Math.max(0, value)).toFixed(1);
}
